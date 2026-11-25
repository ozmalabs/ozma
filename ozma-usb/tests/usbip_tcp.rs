// SPDX-License-Identifier: AGPL-3.0-only
//! USB/IP TCP data-endpoint tests.
//!
//! A loopback TCP listener plays the remote USB/IP server and speaks the real
//! control/URB codec. No kernel module, `usbip` command, or USB device is used.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ozma_usb::usbip_protocol::{
    UsbIpDeviceDescriptor, UsbIpImportReply, UsbIpImportRequest, UsbIpOpCode, UsbIpOpHeader,
    UsbIpStatus, USBIP_PROTOCOL_VERSION,
};
use ozma_usb::usbip_urb::{
    UsbIpCmdSubmit, UsbIpDirection, UsbIpRetSubmit, UsbIpRetUnlink, UsbIpUrb,
    USBIP_URB_HEADER_BYTES, USBIP_URB_UNLINK_CANCELLED,
};
use ozma_usb::{
    LinuxUsbIpDataConfig, LinuxUsbIpKernelDataEndpoint, UsbError, UsbIpAttachment,
    UsbIpAuthorization, UsbIpKernelDataEndpoint, UsbIpToken, UsbIpUrbConnector, UsbIpUrbStream,
};

const REMOTE_BUS: u32 = 2;
const REMOTE_DEV: u32 = 3;
const REMOTE_DEVID: u32 = (REMOTE_BUS << 16) | REMOTE_DEV;
const LOCAL_DEVID: u32 = 0x0007_0001;

fn attachment() -> UsbIpAttachment {
    UsbIpAttachment {
        host: "127.0.0.1".to_owned(),
        bus_id: "1-1".to_owned(),
        port: None,
        vendor_id: Some(0x1d6b),
        product_id: Some(0x0002),
    }
}

fn read_exact(stream: &mut TcpStream, buffer: &mut [u8]) -> bool {
    let mut filled = 0;
    while filled < buffer.len() {
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => return false,
            Ok(read) => filled += read,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return false,
        }
    }
    true
}

fn read_frame(stream: &mut TcpStream) -> Option<UsbIpUrb> {
    let mut header = [0u8; USBIP_URB_HEADER_BYTES];
    if !read_exact(stream, &mut header) {
        return None;
    }
    let frame_len = UsbIpUrb::frame_len_from_header_bounded(&header, 64 * 1024).ok()?;
    let mut frame = vec![0u8; frame_len];
    frame[..USBIP_URB_HEADER_BYTES].copy_from_slice(&header);
    if frame_len > USBIP_URB_HEADER_BYTES
        && !read_exact(stream, &mut frame[USBIP_URB_HEADER_BYTES..])
    {
        return None;
    }
    UsbIpUrb::decode_bounded(&frame, 64 * 1024).ok()
}

fn send_import_reply(stream: &mut TcpStream) -> bool {
    let mut request = [0u8; 40];
    if !read_exact(stream, &mut request) {
        return false;
    }
    let request = match UsbIpImportRequest::decode(&request) {
        Ok(request) => request,
        Err(_) => return false,
    };
    let descriptor = UsbIpDeviceDescriptor::new("/sys/devices/usb2/2-1", request.bus_id)
        .with_ids(0x1d6b, 0x0002)
        .with_numbers(REMOTE_BUS, REMOTE_DEV)
        .with_speed(3);
    let reply = UsbIpImportReply::new(descriptor).encode().unwrap();
    stream.write_all(&reply).is_ok() && stream.flush().is_ok()
}

fn send_refused_import(stream: &mut TcpStream) -> bool {
    let mut request = [0u8; 40];
    if !read_exact(stream, &mut request) {
        return false;
    }
    let header = UsbIpOpHeader::reply(UsbIpOpCode::RepImport, UsbIpStatus::NoDev);
    stream.write_all(&header.encode()).is_ok() && stream.flush().is_ok()
}

fn serve(handler: impl FnOnce(&mut TcpStream) + Send + 'static) -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let join = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        handler(&mut stream);
    });
    (port, join)
}

fn endpoint(port: u16, timeout_ms: u64) -> LinuxUsbIpKernelDataEndpoint {
    LinuxUsbIpKernelDataEndpoint::system().with_config(LinuxUsbIpDataConfig {
        port,
        timeout_ms,
        ..LinuxUsbIpDataConfig::default()
    })
}

fn token() -> UsbIpToken {
    UsbIpToken::new("ozma-peer-token-0123456789").unwrap()
}

#[test]
fn import_then_submit_in_and_out_with_devid_translation() {
    let seen = Arc::new(Mutex::new(Vec::<u32>::new()));
    let server_seen = Arc::clone(&seen);
    let (port, join) = serve(move |stream| {
        assert!(send_import_reply(stream));
        while let Some(urb) = read_frame(stream) {
            match urb {
                UsbIpUrb::CmdSubmit(submit) => {
                    server_seen.lock().unwrap().push(submit.devid);
                    let mut completion = match submit.direction {
                        UsbIpDirection::Out => {
                            let mut completion = UsbIpRetSubmit::new(
                                submit.seqnum,
                                submit.devid,
                                UsbIpDirection::Out,
                                submit.ep,
                                Vec::new(),
                            );
                            completion.actual_length = submit.data.len() as u32;
                            completion
                        }
                        UsbIpDirection::In => UsbIpRetSubmit::new(
                            submit.seqnum,
                            submit.devid,
                            UsbIpDirection::In,
                            submit.ep,
                            vec![0xab; submit.transfer_buffer_length as usize],
                        ),
                    };
                    if submit.direction == UsbIpDirection::Out {
                        completion.devid = REMOTE_DEVID;
                    }
                    let frame = UsbIpUrb::RetSubmit(completion).encode().unwrap();
                    if stream.write_all(&frame).is_err() {
                        break;
                    }
                    let _ = stream.flush();
                }
                UsbIpUrb::CmdUnlink(unlink) => {
                    let reply =
                        UsbIpUrb::RetUnlink(UsbIpRetUnlink::cancelled(unlink.seqnum, unlink.devid));
                    let _ = stream.write_all(&reply.encode().unwrap());
                    let _ = stream.flush();
                }
                _ => break,
            }
        }
    });

    let endpoint = endpoint(port, 1_000);
    let session = endpoint.open(&attachment()).unwrap();

    let out = session
        .submit(&UsbIpCmdSubmit::out(1, LOCAL_DEVID, 2, b"hello".to_vec()))
        .unwrap();
    assert_eq!(out.status, 0);
    assert_eq!(out.devid, LOCAL_DEVID);
    assert_eq!(out.actual_length, 5);

    let input = session
        .submit(&UsbIpCmdSubmit::in_request(2, LOCAL_DEVID, 1, 4))
        .unwrap();
    assert_eq!(input.status, 0);
    assert_eq!(input.devid, LOCAL_DEVID);
    assert_eq!(input.data, vec![0xab; 4]);

    assert_eq!(*seen.lock().unwrap(), vec![REMOTE_DEVID, REMOTE_DEVID]);
    drop(session);
    join.join().unwrap();
}

#[test]
fn unlink_is_forwarded_with_the_remote_devid() {
    let seen = Arc::new(Mutex::new(Vec::<(u32, u32)>::new()));
    let server_seen = Arc::clone(&seen);
    let (port, join) = serve(move |stream| {
        assert!(send_import_reply(stream));
        if let Some(UsbIpUrb::CmdUnlink(unlink)) = read_frame(stream) {
            server_seen
                .lock()
                .unwrap()
                .push((unlink.devid, unlink.unlink_seqnum));
            let reply = UsbIpUrb::RetUnlink(UsbIpRetUnlink::cancelled(unlink.seqnum, unlink.devid));
            let _ = stream.write_all(&reply.encode().unwrap());
            let _ = stream.flush();
        }
    });

    let endpoint = endpoint(port, 1_000);
    let session = endpoint.open(&attachment()).unwrap();
    let reply = session.unlink(LOCAL_DEVID, 42).unwrap();
    assert_eq!(reply.status, USBIP_URB_UNLINK_CANCELLED);
    assert_eq!(*seen.lock().unwrap(), vec![(REMOTE_DEVID, 42)]);
    drop(session);
    join.join().unwrap();
}

#[test]
fn a_silent_remote_returns_a_typed_timeout() {
    let (port, join) = serve(move |stream| {
        assert!(send_import_reply(stream));
        // Consume the submit but never answer it.
        let _ = read_frame(stream);
        // Wait for the client to close so the listener thread can exit.
        let mut byte = [0u8; 1];
        let _ = stream.read(&mut byte);
    });

    let endpoint = endpoint(port, 100);
    let session = endpoint.open(&attachment()).unwrap();
    let error = session
        .submit(&UsbIpCmdSubmit::out(9, LOCAL_DEVID, 2, b"x".to_vec()))
        .unwrap_err();
    assert_eq!(error.status, -110);
    assert!(error.reason.contains("did not complete"));
    drop(session);
    join.join().unwrap();
}

#[test]
fn silent_import_handshake_is_bounded() {
    let (port, join) = serve(move |stream| {
        let mut request = [0u8; 40];
        assert!(read_exact(stream, &mut request));
        let mut byte = [0u8; 1];
        let _ = stream.read(&mut byte);
    });

    let error = endpoint(port, 100).open(&attachment()).err().unwrap();
    assert_eq!(error.kind(), "usbip-protocol");
    assert!(error.to_string().contains("timed out"));
    join.join().unwrap();
}

#[test]
fn refused_import_is_typed_and_opens_no_session() {
    let (port, join) = serve(move |stream| {
        assert!(send_refused_import(stream));
    });

    let error = endpoint(port, 1_000).open(&attachment()).err().unwrap();
    assert_eq!(error.kind(), "usbip-unknown-device");
    assert!(error.to_string().contains("not exported"));
    join.join().unwrap();
}

#[test]
fn malformed_import_status_is_a_protocol_failure() {
    let (port, join) = serve(move |stream| {
        let mut request = [0u8; 40];
        assert!(read_exact(stream, &mut request));
        let mut header = UsbIpOpHeader::reply(UsbIpOpCode::RepImport, UsbIpStatus::Ok);
        header.version = USBIP_PROTOCOL_VERSION + 1;
        let _ = stream.write_all(&header.encode());
    });

    let error = endpoint(port, 1_000).open(&attachment()).err().unwrap();
    assert_eq!(error.kind(), "usbip-protocol");
    join.join().unwrap();
}

#[derive(Debug, Default)]
struct RecordingConnector {
    attempts: AtomicUsize,
    available: AtomicBool,
}

impl UsbIpUrbConnector for RecordingConnector {
    fn available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }

    fn connect(&self, _host: &str, _port: u16) -> Result<Box<dyn UsbIpUrbStream>, UsbError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err(UsbError::InvalidUsbIp {
            field: "host",
            reason: "test connector must not be reached".to_owned(),
        })
    }
}

#[test]
fn authorization_denial_precedes_any_connection_attempt() {
    let connector = Arc::new(RecordingConnector::default());
    connector.available.store(true, Ordering::SeqCst);
    let endpoint = LinuxUsbIpKernelDataEndpoint::with_connector(
        connector.clone() as Arc<dyn UsbIpUrbConnector>
    )
    .with_config(LinuxUsbIpDataConfig {
        port: 3240,
        timeout_ms: 1_000,
        max_transfer_bytes: 1024,
        authorization: Some(UsbIpAuthorization {
            allowed_hosts: vec!["127.0.0.1".to_owned()],
            allowed_keys: vec![],
            allowed_tokens: vec![token()],
        }),
    });

    let error = endpoint.open(&attachment()).err().unwrap();
    assert_eq!(error.kind(), "usbip-unauthorized");
    assert_eq!(connector.attempts.load(Ordering::SeqCst), 0);
    assert!(!error.to_string().contains("ozma-peer-token"));
}

#[test]
fn unavailable_connector_is_reported_without_connecting() {
    let connector = Arc::new(RecordingConnector::default());
    let endpoint = LinuxUsbIpKernelDataEndpoint::with_connector(connector);
    assert!(!endpoint.available());
}

#[test]
#[ignore = "requires OZMA_USBIP_REAL_HOST and a real remote USB/IP export"]
fn real_remote_import_is_environment_gated() {
    let Ok(host) = std::env::var("OZMA_USBIP_REAL_HOST") else {
        eprintln!("set OZMA_USBIP_REAL_HOST/OZMA_USBIP_REAL_BUSID to run this test");
        return;
    };
    let Ok(bus_id) = std::env::var("OZMA_USBIP_REAL_BUSID") else {
        eprintln!("set OZMA_USBIP_REAL_BUSID to run this test");
        return;
    };
    let endpoint = LinuxUsbIpKernelDataEndpoint::system();
    let attachment = UsbIpAttachment {
        host,
        bus_id,
        port: None,
        vendor_id: None,
        product_id: None,
    };
    let session = endpoint.open(&attachment).unwrap();
    assert!(!session
        .submit(&UsbIpCmdSubmit::in_request(1, 0, 1, 8))
        .is_err());
}
