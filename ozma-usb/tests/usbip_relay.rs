// SPDX-License-Identifier: AGPL-3.0-only
//! Bounded USB/IP URB relay tests: pairing, out-of-order completion, unlink
//! timing, endpoint errors, connection cleanup, in-flight ceiling, and idle
//! timeout. All endpoints are in-memory fakes; no kernel module or USB device
//! is touched.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ozma_usb::usbip_relay::{
    serve_data_session, UsbIpDataEndpoint, UsbIpDataError, UsbIpRelayConfig,
    UsbIpRelaySessionOutcome, USBIP_URB_STATUS_EBUSY,
};
use ozma_usb::usbip_server::{UsbIpManualClock, UsbIpSessionEnd, UsbIpTransport};
use ozma_usb::usbip_urb::{
    UsbIpCmdSubmit, UsbIpCmdUnlink, UsbIpDirection, UsbIpRetSubmit, UsbIpRetUnlink, UsbIpUrb,
    USBIP_URB_UNLINK_CANCELLED,
};
use ozma_usb::UsbError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SubmissionRecord {
    seqnum: u32,
    devid: u32,
    direction: UsbIpDirection,
    ep: u32,
    transfer_buffer_length: u32,
    data_len: usize,
}

#[derive(Default)]
struct EndpointState {
    submissions: Vec<SubmissionRecord>,
    unlinks: Vec<(u32, u32)>,
    pending: HashMap<u32, VecDeque<Result<UsbIpRetSubmit, UsbIpDataError>>>,
    completed: usize,
    auto: bool,
    unlink_status: i32,
    complete_on_unlink: bool,
}

/// A controllable endpoint: either echoes a deterministic completion, or waits
/// for the test to release each seqnum in a chosen order.
struct ControlledEndpoint {
    state: Mutex<EndpointState>,
    changed: Condvar,
}

impl ControlledEndpoint {
    fn manual() -> Self {
        Self {
            state: Mutex::new(EndpointState {
                unlink_status: USBIP_URB_UNLINK_CANCELLED,
                ..EndpointState::default()
            }),
            changed: Condvar::new(),
        }
    }

    fn auto() -> Self {
        let endpoint = Self::manual();
        endpoint.state.lock().unwrap().auto = true;
        endpoint
    }

    fn complete_on_unlink(&self) {
        self.state.lock().unwrap().complete_on_unlink = true;
    }

    fn submissions(&self) -> Vec<SubmissionRecord> {
        self.state.lock().unwrap().submissions.clone()
    }

    fn unlinks(&self) -> Vec<(u32, u32)> {
        self.state.lock().unwrap().unlinks.clone()
    }

    fn completed(&self) -> usize {
        self.state.lock().unwrap().completed
    }

    fn allow(&self, seqnum: u32, result: Result<UsbIpRetSubmit, UsbIpDataError>) {
        self.state
            .lock()
            .unwrap()
            .pending
            .entry(seqnum)
            .or_default()
            .push_back(result);
        self.changed.notify_all();
    }
}

impl UsbIpDataEndpoint for ControlledEndpoint {
    fn submit(&self, request: &UsbIpCmdSubmit) -> Result<UsbIpRetSubmit, UsbIpDataError> {
        let auto = {
            let mut state = self.state.lock().unwrap();
            state.submissions.push(SubmissionRecord {
                seqnum: request.seqnum,
                devid: request.devid,
                direction: request.direction,
                ep: request.ep,
                transfer_buffer_length: request.transfer_buffer_length,
                data_len: request.data.len(),
            });
            state.auto
        };
        if auto {
            return Ok(match request.direction {
                UsbIpDirection::Out => {
                    let mut completion = UsbIpRetSubmit::new(
                        request.seqnum,
                        request.devid,
                        request.direction,
                        request.ep,
                        Vec::new(),
                    );
                    completion.actual_length = request.transfer_buffer_length;
                    completion
                }
                UsbIpDirection::In => UsbIpRetSubmit::new(
                    request.seqnum,
                    request.devid,
                    request.direction,
                    request.ep,
                    vec![0x5a; request.transfer_buffer_length as usize],
                ),
            });
        }

        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(result) = state
                .pending
                .get_mut(&request.seqnum)
                .and_then(VecDeque::pop_front)
            {
                state.completed += 1;
                return result;
            }
            state = self.changed.wait(state).unwrap();
        }
    }

    fn unlink(&self, devid: u32, seqnum: u32) -> Result<UsbIpRetUnlink, UsbIpDataError> {
        let (status, complete) = {
            let mut state = self.state.lock().unwrap();
            state.unlinks.push((devid, seqnum));
            (state.unlink_status, state.complete_on_unlink)
        };
        if complete {
            let mut state = self.state.lock().unwrap();
            state
                .pending
                .entry(seqnum)
                .or_default()
                .push_back(Ok(UsbIpRetSubmit::error(seqnum, devid, 0)));
            drop(state);
            self.changed.notify_all();
        }
        Ok(UsbIpRetUnlink::new(0, devid, status))
    }
}

struct FailingEndpoint;

impl UsbIpDataEndpoint for FailingEndpoint {
    fn submit(&self, _request: &UsbIpCmdSubmit) -> Result<UsbIpRetSubmit, UsbIpDataError> {
        Err(UsbIpDataError::new(-5, "device stalled"))
    }

    fn unlink(&self, devid: u32, _seqnum: u32) -> Result<UsbIpRetUnlink, UsbIpDataError> {
        Ok(UsbIpRetUnlink::new(0, devid, USBIP_URB_UNLINK_CANCELLED))
    }
}

/// A full-duplex transport whose input is fed frame-by-frame by the test.
struct ChannelTransport {
    receiver: Receiver<Vec<u8>>,
    pending: Vec<u8>,
    offset: usize,
    output: Arc<Mutex<Vec<u8>>>,
}

impl Read for ChannelTransport {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.offset >= self.pending.len() {
            match self.receiver.try_recv() {
                Ok(chunk) => {
                    self.pending = chunk;
                    self.offset = 0;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "idle"))
                }
                Err(mpsc::TryRecvError::Disconnected) => return Ok(0),
            }
        }
        let available = self.pending.len() - self.offset;
        let read = available.min(buffer.len());
        buffer[..read].copy_from_slice(&self.pending[self.offset..self.offset + read]);
        self.offset += read;
        Ok(read)
    }
}

impl Write for ChannelTransport {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.output.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl UsbIpTransport for ChannelTransport {
    fn peer_host(&self) -> Option<String> {
        Some("test-peer".to_owned())
    }
}

struct Harness {
    sender: Sender<Vec<u8>>,
    output: Arc<Mutex<Vec<u8>>>,
    stop: Arc<AtomicBool>,
    clock: Arc<UsbIpManualClock>,
    join: Option<JoinHandle<Result<UsbIpRelaySessionOutcome, UsbError>>>,
}

impl Harness {
    fn start(
        endpoint: Arc<dyn UsbIpDataEndpoint>,
        config: UsbIpRelayConfig,
        clock_ms: u64,
    ) -> Self {
        let (sender, receiver) = mpsc::channel();
        let output = Arc::new(Mutex::new(Vec::new()));
        let transport = ChannelTransport {
            receiver,
            pending: Vec::new(),
            offset: 0,
            output: Arc::clone(&output),
        };
        let clock = Arc::new(UsbIpManualClock::new(clock_ms));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_clock = Arc::clone(&clock);
        let thread_stop = Arc::clone(&stop);
        let join = thread::spawn(move || {
            let mut transport = transport;
            serve_data_session(
                &mut transport,
                endpoint,
                &config,
                thread_clock.as_ref(),
                thread_stop.as_ref(),
            )
        });
        Self {
            sender,
            output,
            stop,
            clock,
            join: Some(join),
        }
    }

    fn send(&self, urb: &UsbIpUrb) {
        self.sender.send(urb.encode().unwrap()).unwrap();
    }

    fn frames(&self) -> Vec<UsbIpUrb> {
        decode_frames(&self.output.lock().unwrap())
    }

    fn wait_frames(&self, count: usize) -> Vec<UsbIpUrb> {
        wait_until(|| self.frames().len() >= count);
        self.frames()
    }

    fn stop(mut self) -> (Result<UsbIpRelaySessionOutcome, UsbError>, Vec<UsbIpUrb>) {
        self.stop.store(true, Ordering::SeqCst);
        self.finish()
    }

    fn join(mut self) -> (Result<UsbIpRelaySessionOutcome, UsbError>, Vec<UsbIpUrb>) {
        self.finish()
    }

    fn finish(&mut self) -> (Result<UsbIpRelaySessionOutcome, UsbError>, Vec<UsbIpUrb>) {
        let result = self.join.take().unwrap().join().unwrap();
        (result, decode_frames(&self.output.lock().unwrap()))
    }
}

fn decode_frames(bytes: &[u8]) -> Vec<UsbIpUrb> {
    let mut frames = Vec::new();
    let mut offset = 0;
    while offset + 48 <= bytes.len() {
        let Ok(length) = UsbIpUrb::frame_len_from_header(&bytes[offset..]) else {
            break;
        };
        if offset + length > bytes.len() {
            break;
        }
        let Ok(urb) = UsbIpUrb::decode(&bytes[offset..offset + length]) else {
            break;
        };
        frames.push(urb);
        offset += length;
    }
    frames
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for relay condition"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn session_config() -> UsbIpRelayConfig {
    UsbIpRelayConfig {
        max_in_flight: 8,
        idle_timeout_ms: None,
        ..UsbIpRelayConfig::default()
    }
}

fn out_submit(seqnum: u32, data: &[u8]) -> UsbIpCmdSubmit {
    UsbIpCmdSubmit::out(seqnum, 0x0001_0002, 2, data.to_vec())
}

fn in_submit(seqnum: u32, length: u32) -> UsbIpCmdSubmit {
    UsbIpCmdSubmit::in_request(seqnum, 0x0001_0002, 1, length)
}

fn ret_submit(frame: &UsbIpUrb) -> &UsbIpRetSubmit {
    match frame {
        UsbIpUrb::RetSubmit(submit) => submit,
        other => panic!("expected RET_SUBMIT, got {other:?}"),
    }
}

fn ret_unlink(frame: &UsbIpUrb) -> &UsbIpRetUnlink {
    match frame {
        UsbIpUrb::RetUnlink(unlink) => unlink,
        other => panic!("expected RET_UNLINK, got {other:?}"),
    }
}

#[test]
fn one_in_flight_out_submit_round_trips_through_the_endpoint() {
    let endpoint = Arc::new(ControlledEndpoint::auto());
    let harness = Harness::start(endpoint.clone(), session_config(), 0);
    harness.send(&UsbIpUrb::CmdSubmit(out_submit(1, b"hello")));

    let frames = harness.wait_frames(1);
    assert_eq!(frames.len(), 1);
    let completion = ret_submit(&frames[0]);
    assert_eq!(completion.seqnum, 1);
    assert_eq!(completion.status, 0);
    assert_eq!(completion.actual_length, 5);
    assert!(completion.data.is_empty());

    let (result, _) = harness.stop();
    let outcome = result.unwrap();
    assert_eq!(outcome.submits, 1);
    assert_eq!(outcome.submits_dispatched, 1);
    assert_eq!(outcome.ret_submits, 1);
    assert_eq!(outcome.bytes_from_peer > 0, true);
    assert_eq!(outcome.bytes_to_peer > 0, true);
    assert_eq!(
        endpoint.submissions(),
        vec![SubmissionRecord {
            seqnum: 1,
            devid: 0x0001_0002,
            direction: UsbIpDirection::Out,
            ep: 2,
            transfer_buffer_length: 5,
            data_len: 5,
        }]
    );
}

#[test]
fn many_in_flight_completions_are_written_in_endpoint_order() {
    let endpoint = Arc::new(ControlledEndpoint::manual());
    let harness = Harness::start(endpoint.clone(), session_config(), 0);
    for (seqnum, length) in [(1, 1), (2, 2), (3, 3)] {
        harness.send(&UsbIpUrb::CmdSubmit(in_submit(seqnum, length)));
    }
    wait_until(|| endpoint.submissions().len() == 3);

    let completion = |seqnum: u32, length: usize| {
        Ok(UsbIpRetSubmit::new(
            seqnum,
            0x0001_0002,
            UsbIpDirection::In,
            1,
            vec![seqnum as u8; length],
        ))
    };
    endpoint.allow(3, completion(3, 3));
    let frames = harness.wait_frames(1);
    assert_eq!(ret_submit(&frames[0]).seqnum, 3);
    assert_eq!(ret_submit(&frames[0]).data, vec![3, 3, 3]);

    endpoint.allow(1, completion(1, 1));
    let frames = harness.wait_frames(2);
    assert_eq!(ret_submit(&frames[1]).seqnum, 1);
    assert_eq!(ret_submit(&frames[1]).data, vec![1]);

    endpoint.allow(2, completion(2, 2));
    let frames = harness.wait_frames(3);
    let order: Vec<u32> = frames
        .iter()
        .map(|frame| ret_submit(frame).seqnum)
        .collect();
    assert_eq!(order, vec![3, 1, 2]);

    let (result, frames) = harness.stop();
    let outcome = result.unwrap();
    assert_eq!(outcome.submits_dispatched, 3);
    assert_eq!(outcome.ret_submits, 3);
    assert_eq!(frames.len(), 3);
}

#[test]
fn unlink_before_completion_cancels_and_drops_the_late_submit() {
    let endpoint = Arc::new(ControlledEndpoint::manual());
    let harness = Harness::start(endpoint.clone(), session_config(), 0);
    harness.send(&UsbIpUrb::CmdSubmit(out_submit(1, b"data")));
    wait_until(|| endpoint.submissions().len() == 1);

    harness.send(&UsbIpUrb::CmdUnlink(UsbIpCmdUnlink::new(2, 0x0001_0002, 1)));
    let frames = harness.wait_frames(1);
    let reply = ret_unlink(&frames[0]);
    assert_eq!(reply.seqnum, 2);
    assert_eq!(reply.status, USBIP_URB_UNLINK_CANCELLED);
    assert_eq!(endpoint.unlinks(), vec![(0x0001_0002, 1)]);

    let mut completion = UsbIpRetSubmit::new(1, 0x0001_0002, UsbIpDirection::Out, 2, Vec::new());
    completion.actual_length = 4;
    endpoint.allow(1, Ok(completion));
    wait_until(|| endpoint.completed() == 1);
    thread::sleep(Duration::from_millis(50));

    let (result, frames) = harness.stop();
    let outcome = result.unwrap();
    assert_eq!(outcome.ret_unlinks, 1);
    assert_eq!(outcome.ret_submits, 0);
    assert_eq!(outcome.unlink_not_found, 0);
    assert_eq!(frames.len(), 1);
}

#[test]
fn unlink_after_completion_reports_not_found_and_keeps_the_submit() {
    let endpoint = Arc::new(ControlledEndpoint::auto());
    let harness = Harness::start(endpoint.clone(), session_config(), 0);
    harness.send(&UsbIpUrb::CmdSubmit(out_submit(1, b"done")));
    let frames = harness.wait_frames(1);
    assert_eq!(ret_submit(&frames[0]).seqnum, 1);

    harness.send(&UsbIpUrb::CmdUnlink(UsbIpCmdUnlink::new(2, 0x0001_0002, 1)));
    let frames = harness.wait_frames(2);
    let reply = ret_unlink(&frames[1]);
    assert_eq!(reply.seqnum, 2);
    assert_eq!(reply.status, 0);
    assert!(endpoint.unlinks().is_empty());

    let (result, frames) = harness.stop();
    let outcome = result.unwrap();
    assert_eq!(outcome.ret_submits, 1);
    assert_eq!(outcome.ret_unlinks, 1);
    assert_eq!(outcome.unlink_not_found, 1);
    assert_eq!(frames.len(), 2);
}

#[test]
fn in_flight_ceiling_rejects_the_excess_submit_with_ebusy() {
    let endpoint = Arc::new(ControlledEndpoint::manual());
    let config = UsbIpRelayConfig {
        max_in_flight: 1,
        idle_timeout_ms: None,
        ..UsbIpRelayConfig::default()
    };
    let harness = Harness::start(endpoint.clone(), config, 0);
    harness.send(&UsbIpUrb::CmdSubmit(in_submit(1, 4)));
    wait_until(|| endpoint.submissions().len() == 1);

    harness.send(&UsbIpUrb::CmdSubmit(in_submit(2, 4)));
    let frames = harness.wait_frames(1);
    let rejected = ret_submit(&frames[0]);
    assert_eq!(rejected.seqnum, 2);
    assert_eq!(rejected.status, USBIP_URB_STATUS_EBUSY);
    assert!(rejected.data.is_empty());
    assert_eq!(endpoint.submissions().len(), 1);

    endpoint.allow(
        1,
        Ok(UsbIpRetSubmit::new(
            1,
            0x0001_0002,
            UsbIpDirection::In,
            1,
            vec![0xaa; 4],
        )),
    );
    let frames = harness.wait_frames(2);
    assert_eq!(ret_submit(&frames[1]).seqnum, 1);

    let (result, _) = harness.stop();
    let outcome = result.unwrap();
    assert_eq!(outcome.submits, 2);
    assert_eq!(outcome.submits_dispatched, 1);
    assert_eq!(outcome.rejected_in_flight, 1);
    assert_eq!(outcome.ret_submits, 2);
}

#[test]
fn endpoint_error_becomes_a_negative_status_without_invented_bytes() {
    let harness = Harness::start(Arc::new(FailingEndpoint), session_config(), 0);
    harness.send(&UsbIpUrb::CmdSubmit(in_submit(7, 8)));
    let frames = harness.wait_frames(1);
    let completion = ret_submit(&frames[0]);
    assert_eq!(completion.seqnum, 7);
    assert_eq!(completion.status, -5);
    assert_eq!(completion.actual_length, 0);
    assert!(completion.data.is_empty());

    let (result, _) = harness.stop();
    let outcome = result.unwrap();
    assert_eq!(outcome.endpoint_errors, 1);
    assert_eq!(outcome.ret_submits, 1);
}

#[test]
fn disconnect_cleanup_unlinks_every_outstanding_urb() {
    let endpoint = Arc::new(ControlledEndpoint::manual());
    endpoint.complete_on_unlink();
    let harness = Harness::start(endpoint.clone(), session_config(), 0);
    harness.send(&UsbIpUrb::CmdSubmit(out_submit(1, b"pending")));
    wait_until(|| endpoint.submissions().len() == 1);

    let (result, frames) = harness.stop();
    let outcome = result.unwrap();
    assert_eq!(outcome.ret_submits, 0);
    assert_eq!(outcome.cancelled_on_disconnect, 1);
    assert!(frames.is_empty());
    assert_eq!(endpoint.unlinks(), vec![(0x0001_0002, 1)]);
}

#[test]
fn idle_timeout_ends_the_session_and_unlinks_in_flight_urbs() {
    let endpoint = Arc::new(ControlledEndpoint::manual());
    endpoint.complete_on_unlink();
    let config = UsbIpRelayConfig {
        max_in_flight: 8,
        idle_timeout_ms: Some(10),
        ..UsbIpRelayConfig::default()
    };
    let harness = Harness::start(endpoint.clone(), config, 0);
    harness.send(&UsbIpUrb::CmdSubmit(out_submit(1, b"idle")));
    wait_until(|| endpoint.submissions().len() == 1);

    harness.clock.advance(11);
    let (result, frames) = harness.join();
    let outcome = result.unwrap();
    assert_eq!(outcome.end, Some(UsbIpSessionEnd::Timeout));
    assert_eq!(outcome.cancelled_on_disconnect, 1);
    assert_eq!(outcome.ret_submits, 0);
    assert!(frames.is_empty());
    assert_eq!(endpoint.unlinks(), vec![(0x0001_0002, 1)]);
}

#[test]
fn relay_outcome_debug_never_contains_urb_payload() {
    let secret = "relay-secret-payload";
    let harness = Harness::start(Arc::new(ControlledEndpoint::auto()), session_config(), 0);
    harness.send(&UsbIpUrb::CmdSubmit(out_submit(1, secret.as_bytes())));
    harness.wait_frames(1);
    let (result, _) = harness.stop();
    let outcome = result.unwrap();
    let debug = format!("{outcome:?}");
    assert!(!debug.contains(secret));
    assert!(debug.contains("ret_submits"));
}
