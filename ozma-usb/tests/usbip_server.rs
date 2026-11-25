// SPDX-License-Identifier: AGPL-3.0-only
//! USB/IP control-server tests.
//!
//! The protocol/session tests use an in-memory duplex transport and a scripted
//! kernel backend: no socket, `usbip`, kernel module, or device is touched.
//! The final test uses a real loopback TCP listener with the same codec; it
//! still drives only the in-process daemon and a fake kernel backend.

use std::io::{self, Cursor, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ozma_usb::usbip_protocol::{
    UsbIpDevListReply, UsbIpDevListRequest, UsbIpDeviceDescriptor, UsbIpImportReply,
    UsbIpImportRequest, UsbIpInterface, UsbIpOpCode, UsbIpProtocolError, UsbIpStatus,
    OP_REQ_DEVLIST, USBIP_MAX_FRAME_BYTES, USBIP_OP_HEADER_BYTES, USBIP_PROTOCOL_VERSION,
};
use ozma_usb::usbip_server::{
    serve_session, NoUsbIpCredentials, UsbIpClock, UsbIpCredentialSource,
    UsbIpPresentedCredentials, UsbIpServer, UsbIpServerConfig, UsbIpSessionEnd, UsbIpTransport,
};
use ozma_usb::{
    UsbError, UsbIpAuthorization, UsbIpDaemon, UsbIpDaemonConfig, UsbIpHotplugKind,
    UsbIpKernelBackend, UsbIpKey, UsbIpToken, DEFAULT_USBIP_PORT,
};

const TOKEN: &str = "ozma-peer-token-0123456789";
const FINGERPRINT: &str = "SHA256:ab:cd:ef:01:23:45:67";
const PEER: &str = "192.0.2.10";

#[derive(Default)]
struct RecordingKernel {
    calls: Mutex<Vec<String>>,
    attach_port: u16,
}

impl RecordingKernel {
    fn new(attach_port: u16) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            attach_port,
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn record(&self, call: impl Into<String>) {
        self.calls.lock().unwrap().push(call.into());
    }
}

impl UsbIpKernelBackend for RecordingKernel {
    fn available(&self) -> bool {
        true
    }

    fn export(&self, bus_id: &str) -> Result<(), UsbError> {
        self.record(format!("export {bus_id}"));
        Ok(())
    }

    fn unexport(&self, bus_id: &str) -> Result<(), UsbError> {
        self.record(format!("unexport {bus_id}"));
        Ok(())
    }

    fn attach(&self, host: &str, bus_id: &str) -> Result<u16, UsbError> {
        self.record(format!("attach {host} {bus_id}"));
        Ok(self.attach_port)
    }

    fn detach(&self, port: u16) -> Result<(), UsbError> {
        self.record(format!("detach {port}"));
        Ok(())
    }
}

/// Exports work, but `attach` keeps the trait default: typed
/// `usbip-kernel-required`.
#[derive(Default)]
struct ExportOnlyKernel {
    calls: Mutex<Vec<String>>,
}

impl ExportOnlyKernel {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl UsbIpKernelBackend for ExportOnlyKernel {
    fn available(&self) -> bool {
        true
    }

    fn export(&self, bus_id: &str) -> Result<(), UsbError> {
        self.calls.lock().unwrap().push(format!("export {bus_id}"));
        Ok(())
    }
}

struct Duplex {
    input: Cursor<Vec<u8>>,
    output: Arc<Mutex<Vec<u8>>>,
    idle_on_empty: bool,
    host: Option<String>,
}

impl Duplex {
    fn new(input: Vec<u8>, host: &str) -> (Self, Arc<Mutex<Vec<u8>>>) {
        let output = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                input: Cursor::new(input),
                output: Arc::clone(&output),
                idle_on_empty: false,
                host: Some(host.to_owned()),
            },
            output,
        )
    }

    fn idle(host: &str) -> (Self, Arc<Mutex<Vec<u8>>>) {
        let output = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                input: Cursor::new(Vec::new()),
                output: Arc::clone(&output),
                idle_on_empty: true,
                host: Some(host.to_owned()),
            },
            output,
        )
    }
}

impl Read for Duplex {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.input.read(buffer)?;
        if read == 0 && self.idle_on_empty {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "idle"));
        }
        Ok(read)
    }
}

impl Write for Duplex {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.output.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl UsbIpTransport for Duplex {
    fn peer_host(&self) -> Option<String> {
        self.host.clone()
    }
}

struct StaticCredentials {
    token: Option<String>,
    fingerprint: Option<String>,
}

impl StaticCredentials {
    fn new(token: Option<&str>, fingerprint: Option<&str>) -> Self {
        Self {
            token: token.map(str::to_owned),
            fingerprint: fingerprint.map(str::to_owned),
        }
    }
}

impl UsbIpCredentialSource for StaticCredentials {
    fn credentials(&self, _peer_host: &str) -> UsbIpPresentedCredentials {
        let mut presented = UsbIpPresentedCredentials::new();
        if let Some(token) = &self.token {
            presented = presented.with_token(token.clone());
        }
        if let Some(fingerprint) = &self.fingerprint {
            presented = presented.with_key_fingerprint(fingerprint.clone());
        }
        presented
    }
}

/// Advances on every read so idle-deadline tests terminate deterministically.
#[derive(Debug, Default)]
struct TickingClock {
    millis: AtomicU64,
}

impl UsbIpClock for TickingClock {
    fn now_millis(&self) -> u64 {
        self.millis.fetch_add(1, Ordering::SeqCst)
    }
}

fn descriptor(bus_id: &str) -> UsbIpDeviceDescriptor {
    UsbIpDeviceDescriptor::new(format!("/sys/devices/usb1/{bus_id}"), bus_id)
        .with_numbers(1, 2)
        .with_speed(3)
        .with_ids(0x1d6b, 0x0002)
        .with_bcd_device(0x0510)
        .with_device_class(9, 0, 3)
        .with_configuration(1, 1)
        .with_interfaces(vec![
            UsbIpInterface::new(9, 0, 0),
            UsbIpInterface::new(3, 1, 2),
        ])
}

fn server_config() -> UsbIpServerConfig {
    UsbIpServerConfig {
        max_connections: 4,
        max_frame_bytes: USBIP_MAX_FRAME_BYTES,
        idle_timeout_ms: None,
    }
}

fn started_daemon() -> UsbIpDaemon {
    let daemon =
        UsbIpDaemon::new(UsbIpDaemonConfig::server("127.0.0.1", DEFAULT_USBIP_PORT)).unwrap();
    daemon.start().unwrap();
    daemon
}

fn with_kernel(daemon: UsbIpDaemon, kernel: Arc<dyn UsbIpKernelBackend>) -> UsbIpDaemon {
    daemon.with_kernel(kernel)
}

fn serve(
    daemon: &UsbIpDaemon,
    input: Vec<u8>,
    config: &UsbIpServerConfig,
    credentials: &dyn UsbIpCredentialSource,
) -> (Result<ozma_usb::UsbIpSessionOutcome, UsbError>, Vec<u8>) {
    serve_with_clock(daemon, input, config, credentials, &TickingClock::default())
}

fn serve_with_clock(
    daemon: &UsbIpDaemon,
    input: Vec<u8>,
    config: &UsbIpServerConfig,
    credentials: &dyn UsbIpCredentialSource,
    clock: &dyn UsbIpClock,
) -> (Result<ozma_usb::UsbIpSessionOutcome, UsbError>, Vec<u8>) {
    let (mut duplex, output) = Duplex::new(input, PEER);
    let stop = AtomicBool::new(false);
    let result = serve_session(&mut duplex, daemon, config, credentials, clock, &stop);
    let captured = output.lock().unwrap().clone();
    (result, captured)
}

#[test]
fn devlist_empty_is_answered_with_no_invented_devices() {
    let daemon = started_daemon();
    let request = UsbIpDevListRequest::new().encode().unwrap().to_vec();
    let (result, output) = serve(&daemon, request, &server_config(), &NoUsbIpCredentials);

    let outcome = result.unwrap();
    assert_eq!(outcome.version, Some(USBIP_PROTOCOL_VERSION));
    assert_eq!(outcome.devlist_requests, 1);
    assert_eq!(outcome.devices_listed, 0);
    assert_eq!(outcome.end, Some(UsbIpSessionEnd::PeerClosed));
    assert!(outcome.attached_bus_id.is_none());

    let reply = UsbIpDevListReply::decode(&output).unwrap();
    assert_eq!(reply.header.status, UsbIpStatus::Ok);
    assert!(reply.devices.is_empty());
}

#[test]
fn devlist_lists_only_exports_with_a_registered_descriptor() {
    let kernel = Arc::new(RecordingKernel::new(7));
    let daemon = with_kernel(started_daemon(), kernel.clone());
    daemon
        .export_with_descriptor("1-1", descriptor("1-1"))
        .unwrap();
    // An export without a descriptor must not be advertised with invented
    // path/bus/device/interface values.
    daemon.export("1-2").unwrap();
    assert_eq!(daemon.list().unwrap().len(), 2);
    assert_eq!(daemon.device_list().unwrap().len(), 1);

    let request = UsbIpDevListRequest::new().encode().unwrap().to_vec();
    let (result, output) = serve(&daemon, request, &server_config(), &NoUsbIpCredentials);
    let outcome = result.unwrap();
    assert_eq!(outcome.devices_listed, 1);

    let reply = UsbIpDevListReply::decode(&output).unwrap();
    assert_eq!(reply.devices.len(), 1);
    assert_eq!(reply.devices[0], descriptor("1-1"));
    assert_eq!(kernel.calls(), vec!["export 1-1", "export 1-2"]);
}

#[test]
fn authorized_import_returns_busid_only_after_a_real_kernel_attach() {
    let kernel = Arc::new(RecordingKernel::new(7));
    let daemon = with_kernel(started_daemon(), kernel.clone());
    daemon
        .export_with_descriptor("1-1", descriptor("1-1"))
        .unwrap();

    let request = UsbIpImportRequest::new("1-1").encode().unwrap();
    let (result, output) = serve(&daemon, request, &server_config(), &NoUsbIpCredentials);
    let outcome = result.unwrap();
    assert_eq!(outcome.import_requests, 1);
    assert_eq!(outcome.attached_bus_id.as_deref(), Some("1-1"));
    assert_eq!(outcome.end, Some(UsbIpSessionEnd::PeerClosed));

    let reply = UsbIpImportReply::decode(&output).unwrap();
    assert_eq!(reply.header.status, UsbIpStatus::Ok);
    let imported = reply.device.unwrap();
    assert_eq!(imported.bus_id, "1-1");
    assert_eq!(imported.path, descriptor("1-1").path);
    assert_eq!(imported.vendor_id, 0x1d6b);
    assert_eq!(imported.num_interfaces, 2);
    assert!(imported.interfaces.is_empty());

    assert_eq!(kernel.calls(), vec!["export 1-1", "attach 192.0.2.10 1-1"]);
    let status = daemon.status();
    assert_eq!(status.attached.len(), 1);
    assert_eq!(status.attached[0].port, Some(7));
    let events = daemon.hotplug_events();
    assert_eq!(
        events.iter().map(|event| event.kind).collect::<Vec<_>>(),
        vec![UsbIpHotplugKind::Export, UsbIpHotplugKind::Attach]
    );
    assert_eq!(events[1].bus_id, "1-1");
    assert_eq!(events[1].host.as_deref(), Some(PEER));
    assert_eq!(events[1].port, Some(7));
}

#[test]
fn import_is_denied_by_the_same_host_token_and_fingerprint_rules() {
    let authorization = UsbIpAuthorization {
        allowed_hosts: vec![PEER.to_owned()],
        allowed_keys: vec![UsbIpKey::new(FINGERPRINT).unwrap()],
        allowed_tokens: vec![UsbIpToken::new(TOKEN).unwrap()],
    };
    let kernel = Arc::new(RecordingKernel::new(7));
    let daemon = UsbIpDaemon::new(
        UsbIpDaemonConfig::server("127.0.0.1", DEFAULT_USBIP_PORT)
            .with_authorization(authorization),
    )
    .unwrap()
    .with_kernel(kernel.clone());
    daemon.start().unwrap();
    daemon
        .export_with_descriptor("1-1", descriptor("1-1"))
        .unwrap();

    let request = UsbIpImportRequest::new("1-1").encode().unwrap();
    let cases = [
        (
            PEER,
            StaticCredentials::new(Some("wrong-token-0000000000"), Some(FINGERPRINT)),
            "wrong token",
        ),
        (
            PEER,
            StaticCredentials::new(None, Some(FINGERPRINT)),
            "missing token",
        ),
        (
            PEER,
            StaticCredentials::new(Some(TOKEN), Some("SHA256:00:00:00:00:00:00:00:00")),
            "wrong fingerprint",
        ),
        (
            PEER,
            StaticCredentials::new(Some(TOKEN), None),
            "missing fingerprint",
        ),
    ];
    for (host, credentials, label) in cases {
        let (mut duplex, output) = Duplex::new(request.clone(), host);
        let stop = AtomicBool::new(false);
        let error = serve_session(
            &mut duplex,
            &daemon,
            &server_config(),
            &credentials,
            &TickingClock::default(),
            &stop,
        )
        .unwrap_err();
        assert_eq!(error.kind(), "usbip-unauthorized", "{label}");
        assert!(output.lock().unwrap().is_empty(), "{label} wrote a frame");
        if let Some(token) = credentials.token.as_deref() {
            assert!(!error.to_string().contains(token), "{label} leaked a token");
        }
        if let Some(fingerprint) = credentials.fingerprint.as_deref() {
            assert!(
                !error.to_string().contains(fingerprint),
                "{label} leaked a fingerprint"
            );
        }
    }

    // Host allow-list denial with otherwise correct credentials.
    let (mut duplex, output) = Duplex::new(request.clone(), "192.0.2.99");
    let stop = AtomicBool::new(false);
    let error = serve_session(
        &mut duplex,
        &daemon,
        &server_config(),
        &StaticCredentials::new(Some(TOKEN), Some(FINGERPRINT)),
        &TickingClock::default(),
        &stop,
    )
    .unwrap_err();
    assert_eq!(error.kind(), "usbip-unauthorized");
    assert!(output.lock().unwrap().is_empty());

    // No denied request reached the kernel.
    assert_eq!(kernel.calls(), vec!["export 1-1"]);
    assert!(daemon.status().attached.is_empty());
    assert!(daemon.connections().is_empty());

    // Accepted credentials are never serialized or printed.
    let presented = UsbIpPresentedCredentials::new()
        .with_token(TOKEN)
        .with_key_fingerprint(FINGERPRINT);
    let debug = format!("{presented:?}");
    assert!(!debug.contains(TOKEN));
    assert!(!debug.contains(FINGERPRINT));
    assert!(debug.contains("token_presented: true"));
}

#[test]
fn import_without_a_kernel_attach_is_typed_and_returns_no_device() {
    let kernel = Arc::new(ExportOnlyKernel::default());
    let daemon = with_kernel(started_daemon(), kernel.clone());
    daemon
        .export_with_descriptor("1-1", descriptor("1-1"))
        .unwrap();

    let request = UsbIpImportRequest::new("1-1").encode().unwrap();
    let (result, output) = serve(&daemon, request, &server_config(), &NoUsbIpCredentials);
    let error = result.unwrap_err();
    assert_eq!(error.kind(), "usbip-kernel-required");
    assert!(error.to_string().contains("vhci_hcd"));

    // Exactly an 8-byte error header, no 312-byte device body.
    assert_eq!(output.len(), USBIP_OP_HEADER_BYTES);
    let reply = UsbIpImportReply::decode(&output).unwrap();
    assert_eq!(reply.header.status, UsbIpStatus::DevErr);
    assert!(reply.device.is_none());
    assert!(!output.windows(3).any(|window| window == b"1-1"));

    assert_eq!(kernel.calls(), vec!["export 1-1"]);
    assert!(daemon.status().attached.is_empty());
    assert_eq!(
        daemon
            .hotplug_events()
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        vec![UsbIpHotplugKind::Export]
    );
}

#[test]
fn import_of_an_unknown_or_unregistered_busid_is_typed() {
    let kernel = Arc::new(RecordingKernel::new(7));
    let daemon = with_kernel(started_daemon(), kernel.clone());
    daemon
        .export_with_descriptor("1-1", descriptor("1-1"))
        .unwrap();
    // Exported without a descriptor, so it cannot be imported by name.
    daemon.export("1-2").unwrap();

    for bus_id in ["1-9", "1-2"] {
        let request = UsbIpImportRequest::new(bus_id).encode().unwrap();
        let (result, output) = serve(&daemon, request, &server_config(), &NoUsbIpCredentials);
        let error = result.unwrap_err();
        assert_eq!(error.kind(), "usbip-unknown-device", "{bus_id}");
        assert_eq!(output.len(), USBIP_OP_HEADER_BYTES);
        let reply = UsbIpImportReply::decode(&output).unwrap();
        assert_eq!(reply.header.status, UsbIpStatus::NoDev);
        assert!(reply.device.is_none());
    }
    assert_eq!(kernel.calls(), vec!["export 1-1", "export 1-2"]);
    assert!(daemon.status().attached.is_empty());
}

#[test]
fn bad_version_is_rejected_with_an_error_reply_and_never_negotiated() {
    let daemon = started_daemon();
    let mut request = UsbIpDevListRequest::new().encode().unwrap().to_vec();
    request[1] = 0x10; // 0x0110

    let (result, output) = serve(&daemon, request, &server_config(), &NoUsbIpCredentials);
    let error = result.unwrap_err();
    assert_eq!(error.kind(), "usbip-protocol");
    match error {
        UsbError::UsbIpProtocol(UsbIpProtocolError::UnsupportedVersion { version, .. }) => {
            assert_eq!(version, 0x0110);
        }
        other => panic!("unexpected error {other:?}"),
    }

    let reply = UsbIpDevListReply::decode(&output).unwrap();
    assert_eq!(reply.header.version, USBIP_PROTOCOL_VERSION);
    assert_eq!(reply.header.opcode, UsbIpOpCode::RepDevList);
    assert_eq!(reply.header.status, UsbIpStatus::Error);
    assert!(reply.devices.is_empty());
    assert!(daemon.connections().is_empty());
}

#[test]
fn unknown_opcode_and_truncated_frames_are_typed() {
    let daemon = started_daemon();
    let header = [0x01, 0x11, 0x99, 0x99, 0, 0, 0, 0];
    let (result, output) = serve(
        &daemon,
        header.to_vec(),
        &server_config(),
        &NoUsbIpCredentials,
    );
    let error = result.unwrap_err();
    assert_eq!(error.kind(), "usbip-protocol");
    assert!(matches!(
        error,
        UsbError::UsbIpProtocol(UsbIpProtocolError::UnknownOpcode { code: 0x9999 })
    ));
    assert!(output.is_empty());

    // Four bytes of a header, then EOF.
    let (result, output) = serve(
        &daemon,
        vec![0x01, 0x11, 0x80, 0x05],
        &server_config(),
        &NoUsbIpCredentials,
    );
    let error = result.unwrap_err();
    assert!(matches!(
        error,
        UsbError::UsbIpProtocol(UsbIpProtocolError::Truncated {
            needed: 8,
            available: 4,
        })
    ));
    assert!(output.is_empty());

    // Complete import header but a short bus-id body.
    let mut short = UsbIpImportRequest::new("1-1").encode().unwrap();
    short.truncate(USBIP_OP_HEADER_BYTES + 8);
    let (result, _) = serve(&daemon, short, &server_config(), &NoUsbIpCredentials);
    let error = result.unwrap_err();
    assert!(matches!(
        error,
        UsbError::UsbIpProtocol(UsbIpProtocolError::Truncated {
            needed: 32,
            available: 8,
        })
    ));
    assert!(daemon.connections().is_empty());
}

#[test]
fn a_caller_frame_ceiling_rejects_an_import_before_the_body_is_read() {
    let daemon = started_daemon();
    let config = UsbIpServerConfig {
        max_connections: 4,
        max_frame_bytes: 16,
        idle_timeout_ms: None,
    };
    let request = UsbIpImportRequest::new("1-1").encode().unwrap();
    let (result, output) = serve(&daemon, request, &config, &NoUsbIpCredentials);
    let error = result.unwrap_err();
    assert_eq!(error.kind(), "usbip-protocol");
    assert!(matches!(
        error,
        UsbError::UsbIpProtocol(UsbIpProtocolError::Oversized {
            field: "frame",
            limit: 16,
            requested: 40,
        })
    ));
    assert!(output.is_empty());
}

#[test]
fn idle_timeout_ends_a_silent_session() {
    let daemon = started_daemon();
    let config = UsbIpServerConfig {
        max_connections: 4,
        max_frame_bytes: USBIP_MAX_FRAME_BYTES,
        idle_timeout_ms: Some(5),
    };
    let (mut duplex, output) = Duplex::idle(PEER);
    let stop = AtomicBool::new(false);
    let outcome = serve_session(
        &mut duplex,
        &daemon,
        &config,
        &NoUsbIpCredentials,
        &TickingClock::default(),
        &stop,
    )
    .unwrap();
    assert_eq!(outcome.end, Some(UsbIpSessionEnd::Timeout));
    assert!(outcome.version.is_none());
    assert!(output.lock().unwrap().is_empty());
    assert!(daemon.connections().is_empty());
}

/// A transport that holds its session open until released.
struct BlockingTransport {
    release: Arc<AtomicBool>,
    output: Arc<Mutex<Vec<u8>>>,
}

impl Read for BlockingTransport {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        if self.release.load(Ordering::SeqCst) {
            return Ok(0);
        }
        thread::sleep(Duration::from_millis(1));
        Err(io::Error::new(io::ErrorKind::WouldBlock, "held"))
    }
}

impl Write for BlockingTransport {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.output.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl UsbIpTransport for BlockingTransport {
    fn peer_host(&self) -> Option<String> {
        Some(PEER.to_owned())
    }
}

#[test]
fn concurrent_sessions_are_bounded_by_the_daemon_ceiling() {
    let config = UsbIpDaemonConfig::server("127.0.0.1", DEFAULT_USBIP_PORT).with_limits(1, 8);
    let daemon = UsbIpDaemon::new(config).unwrap();
    daemon.start().unwrap();
    let release = Arc::new(AtomicBool::new(false));
    let stop = AtomicBool::new(false);
    let clock = TickingClock::default();
    let server = server_config();

    thread::scope(|scope| {
        let release_first = Arc::clone(&release);
        let first = scope.spawn(|| {
            let mut transport = BlockingTransport {
                release: release_first,
                output: Arc::new(Mutex::new(Vec::new())),
            };
            serve_session(
                &mut transport,
                &daemon,
                &server,
                &NoUsbIpCredentials,
                &clock,
                &stop,
            )
        });

        for _ in 0..200 {
            if daemon.connections().len() == 1 {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(daemon.connections().len(), 1, "first session did not hold");

        let (mut second, output) =
            Duplex::new(UsbIpDevListRequest::new().encode().unwrap().to_vec(), PEER);
        let error = serve_session(
            &mut second,
            &daemon,
            &server,
            &NoUsbIpCredentials,
            &clock,
            &stop,
        )
        .unwrap_err();
        assert_eq!(error.kind(), "usbip-connection-limit");
        assert!(output.lock().unwrap().is_empty());
        assert_eq!(daemon.connections().len(), 1);

        release.store(true, Ordering::SeqCst);
        let outcome = first.join().unwrap().unwrap();
        assert_eq!(outcome.end, Some(UsbIpSessionEnd::PeerClosed));
    });

    assert!(daemon.connections().is_empty());
}

#[test]
fn loopback_tcp_serves_the_same_codec_without_a_kernel_attach_claim() {
    let kernel = Arc::new(RecordingKernel::new(9));
    let daemon = Arc::new(with_kernel(started_daemon(), kernel.clone()));
    daemon
        .export_with_descriptor("1-1", descriptor("1-1"))
        .unwrap();

    let server = UsbIpServer::bind_tcp(
        (std::net::Ipv4Addr::LOCALHOST, 0),
        Arc::clone(&daemon),
        server_config(),
    )
    .unwrap();
    let address = server.local_addr().unwrap();
    let handle = server.start().unwrap();

    let mut client = TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    client
        .write_all(&UsbIpDevListRequest::new().encode().unwrap())
        .unwrap();
    let mut devlist = [0u8; 12 + 320];
    client.read_exact(&mut devlist[..12]).unwrap();
    let ndev = u32::from_be_bytes([devlist[8], devlist[9], devlist[10], devlist[11]]);
    assert_eq!(ndev, 1);
    client.read_exact(&mut devlist[12..12 + 320]).unwrap();
    let reply = UsbIpDevListReply::decode(&devlist).unwrap();
    assert_eq!(reply.devices.len(), 1);
    assert_eq!(reply.devices[0].bus_id, "1-1");

    client
        .write_all(&UsbIpImportRequest::new("1-1").encode().unwrap())
        .unwrap();
    let mut import = [0u8; 320];
    client.read_exact(&mut import).unwrap();
    let reply = UsbIpImportReply::decode(&import).unwrap();
    assert_eq!(reply.header.status, UsbIpStatus::Ok);
    let imported = reply.device.unwrap();
    assert_eq!(imported.bus_id, "1-1");
    assert_eq!(imported.vendor_id, 0x1d6b);

    drop(client);
    let stats = handle.stop();
    assert!(stats.connections_accepted >= 1);
    assert_eq!(stats.devlist_requests, 1);
    assert_eq!(stats.import_requests, 1);
    assert_eq!(stats.imports, 1);
    assert_eq!(stats.import_failures, 0);
    assert!(stats.bytes_read > 0);
    assert!(stats.bytes_written >= 332);

    assert_eq!(kernel.calls(), vec!["export 1-1", "attach 127.0.0.1 1-1"]);
    assert_eq!(daemon.status().attached.len(), 1);
}

#[test]
fn server_configuration_is_bounded() {
    let config = UsbIpServerConfig {
        max_connections: 0,
        ..server_config()
    };
    assert_eq!(config.validate().unwrap_err().kind(), "invalid-usbip");
    let config = UsbIpServerConfig {
        max_frame_bytes: USBIP_OP_HEADER_BYTES - 1,
        ..server_config()
    };
    assert_eq!(config.validate().unwrap_err().kind(), "invalid-usbip");
    let config = UsbIpServerConfig {
        idle_timeout_ms: Some(0),
        ..server_config()
    };
    assert_eq!(config.validate().unwrap_err().kind(), "invalid-usbip");

    let daemon = started_daemon();
    let config = UsbIpServerConfig {
        max_connections: 0,
        ..server_config()
    };
    assert!(
        UsbIpServer::bind_tcp((std::net::Ipv4Addr::LOCALHOST, 0), Arc::new(daemon), config)
            .is_err()
    );
}

#[test]
fn a_stopped_daemon_rejects_control_sessions() {
    let daemon =
        UsbIpDaemon::new(UsbIpDaemonConfig::server("127.0.0.1", DEFAULT_USBIP_PORT)).unwrap();
    let (result, output) = serve(
        &daemon,
        UsbIpDevListRequest::new().encode().unwrap().to_vec(),
        &server_config(),
        &NoUsbIpCredentials,
    );
    assert_eq!(result.unwrap_err().kind(), "usbip-daemon-stopped");
    assert!(output.is_empty());
}

#[test]
fn opcode_constants_are_the_control_plane_requests() {
    assert_eq!(OP_REQ_DEVLIST, 0x8005);
    assert_eq!(UsbIpOpCode::ReqImport.raw(), 0x8003);
}
