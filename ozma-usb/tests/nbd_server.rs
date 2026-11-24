// SPDX-License-Identifier: AGPL-3.0-only
//! NBD server negotiation, bounds, read-only, disconnect, malformed frames,
//! injected transport/clock, and concurrent clients.
//!
//! The protocol tests use an in-memory duplex (no socket). The concurrency and
//! listener tests use loopback TCP/Unix sockets only; no block device, mount,
//! or external NBD client is involved.

use std::io::{self, Cursor, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ozma_usb::nbd::{
    NBD_FLAG_C_FIXED_NEWSTYLE, NBD_FLAG_C_NO_ZEROES, NBD_FLAG_FIXED_NEWSTYLE, NBD_FLAG_NO_ZEROES,
    NBD_FLAG_READ_ONLY, NBD_INFO_BLOCK_SIZE, NBD_INFO_NAME, NBD_MAGIC, NBD_NEWSTYLE_MAGIC,
    NBD_OPT_ABORT, NBD_OPT_EXPORT_NAME, NBD_OPT_GO, NBD_OPT_LIST, NBD_REP_ACK, NBD_REP_INFO,
    NBD_REP_MAGIC, NBD_REP_SERVER, NBD_REQUEST_HEADER_LEN,
};
use ozma_usb::nbd_server::{
    serve_session, ManualClock, NbdClock, NbdExportCatalog, NbdExportSource, NbdMemoryImage,
    NbdServer, NbdServerConfig, NbdSessionEnd, NbdTransport,
};
use ozma_usb::{NbdRequest, UsbError};

/// In-memory duplex: scripted input plus captured output.
struct Duplex {
    input: Cursor<Vec<u8>>,
    output: Arc<Mutex<Vec<u8>>>,
    idle_on_empty: bool,
}

impl Duplex {
    fn new(input: Vec<u8>) -> (Self, Arc<Mutex<Vec<u8>>>) {
        let output = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                input: Cursor::new(input),
                output: Arc::clone(&output),
                idle_on_empty: false,
            },
            output,
        )
    }

    fn idle(input: Vec<u8>) -> Self {
        Self {
            input: Cursor::new(input),
            output: Arc::new(Mutex::new(Vec::new())),
            idle_on_empty: true,
        }
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

impl NbdTransport for Duplex {}

fn handshake_prefix() -> Vec<u8> {
    (NBD_FLAG_C_FIXED_NEWSTYLE | NBD_FLAG_C_NO_ZEROES)
        .to_be_bytes()
        .to_vec()
}

fn option(option: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&NBD_NEWSTYLE_MAGIC.to_be_bytes());
    bytes.extend_from_slice(&option.to_be_bytes());
    bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn export_name_option(name: &str) -> Vec<u8> {
    option(NBD_OPT_EXPORT_NAME, name.as_bytes())
}

fn go_payload(name: &str, infos: &[u16]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(name.len() as u32).to_be_bytes());
    payload.extend_from_slice(name.as_bytes());
    payload.extend_from_slice(&(infos.len() as u16).to_be_bytes());
    for info in infos {
        payload.extend_from_slice(&info.to_be_bytes());
    }
    payload
}

fn read_reply(output: &[u8], cursor: &mut usize) -> (u32, u64) {
    let header = &output[*cursor..*cursor + 16];
    assert_eq!(
        u32::from_be_bytes(header[0..4].try_into().unwrap()),
        0x6744_6698
    );
    let error = u32::from_be_bytes(header[4..8].try_into().unwrap());
    let handle = u64::from_be_bytes(header[8..16].try_into().unwrap());
    *cursor += 16;
    (error, handle)
}

fn option_reply(output: &[u8], cursor: &mut usize) -> (u32, u32, Vec<u8>) {
    let header = &output[*cursor..*cursor + 20];
    assert_eq!(
        u64::from_be_bytes(header[0..8].try_into().unwrap()),
        NBD_REP_MAGIC
    );
    let option = u32::from_be_bytes(header[8..12].try_into().unwrap());
    let reply = u32::from_be_bytes(header[12..16].try_into().unwrap());
    let length = u32::from_be_bytes(header[16..20].try_into().unwrap()) as usize;
    *cursor += 20;
    let payload = output[*cursor..*cursor + length].to_vec();
    *cursor += length;
    (option, reply, payload)
}

fn configured(max_request_bytes: u32) -> NbdServerConfig {
    NbdServerConfig {
        max_request_bytes,
        max_clients: 4,
        handshake_timeout_ms: 10_000,
        idle_timeout_ms: Some(30_000),
        option_max_bytes: 64 * 1024,
    }
}

fn read_only_catalog(data: Vec<u8>) -> NbdExportCatalog {
    NbdExportCatalog::new(vec![NbdExportSource::memory("media", data, true).unwrap()]).unwrap()
}

#[test]
fn injected_transport_handshake_export_read_and_disconnect() {
    let data: Vec<u8> = (0..4096u32).map(|value| value as u8).collect();
    let catalog = read_only_catalog(data.clone());
    let mut script = handshake_prefix();
    script.extend(export_name_option("media"));
    script.extend(
        NbdRequest::Read {
            handle: 11,
            offset: 16,
            length: 16,
        }
        .encode(),
    );
    script.extend(NbdRequest::Disconnect { handle: 12 }.encode());
    let (mut duplex, output) = Duplex::new(script);
    let clock = ManualClock::new(1_000);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let outcome = serve_session(&mut duplex, &catalog, &configured(4096), &clock, &stop).unwrap();

    let output = output.lock().unwrap().clone();
    assert_eq!(&output[..8], NBD_MAGIC);
    assert_eq!(
        u64::from_be_bytes(output[8..16].try_into().unwrap()),
        NBD_NEWSTYLE_MAGIC
    );
    let server_flags = u16::from_be_bytes(output[16..18].try_into().unwrap());
    assert_eq!(server_flags, NBD_FLAG_FIXED_NEWSTYLE | NBD_FLAG_NO_ZEROES);

    let export = &output[18..];
    assert_eq!(u64::from_be_bytes(export[0..8].try_into().unwrap()), 4096);
    let transmission = u16::from_be_bytes(export[8..10].try_into().unwrap());
    assert_ne!(transmission & NBD_FLAG_READ_ONLY as u16, 0);

    let mut cursor = 10;
    let (error, handle) = read_reply(export, &mut cursor);
    assert_eq!(error, 0);
    assert_eq!(handle, 11);
    assert_eq!(&export[cursor..cursor + 16], &data[16..32]);
    assert_eq!(outcome.export.as_deref(), Some("media"));
    assert_eq!(outcome.end, Some(NbdSessionEnd::Disconnect));
    assert_eq!(outcome.read_requests, 1);
    assert_eq!(outcome.disconnect_requests, 1);
    assert_eq!(outcome.bytes_read, 16);
}

#[test]
fn list_info_go_and_empty_export_name() {
    let catalog = NbdExportCatalog::new(vec![
        NbdExportSource::memory("first", vec![1u8; 512], true).unwrap(),
        NbdExportSource::memory("second", vec![2u8; 1024], false)
            .unwrap()
            .with_description("second export"),
    ])
    .unwrap();
    let mut script = handshake_prefix();
    script.extend(option(NBD_OPT_LIST, &[]));
    script.extend(option(
        NBD_OPT_GO,
        &go_payload("second", &[NBD_INFO_NAME, NBD_INFO_BLOCK_SIZE]),
    ));
    script.extend(NbdRequest::Disconnect { handle: 1 }.encode());
    let (mut duplex, output) = Duplex::new(script);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let outcome = serve_session(
        &mut duplex,
        &catalog,
        &configured(4096),
        &ManualClock::new(0),
        &stop,
    )
    .unwrap();
    assert_eq!(outcome.export.as_deref(), Some("second"));

    let output = output.lock().unwrap().clone();
    let mut cursor = 18;
    let mut listed = Vec::new();
    loop {
        let (option, reply, payload) = option_reply(&output, &mut cursor);
        assert_eq!(option, NBD_OPT_LIST);
        if reply == NBD_REP_ACK {
            break;
        }
        assert_eq!(reply, NBD_REP_SERVER);
        let name_len = u32::from_be_bytes(payload[0..4].try_into().unwrap()) as usize;
        listed.push(String::from_utf8(payload[4..4 + name_len].to_vec()).unwrap());
    }
    assert_eq!(listed, vec!["first", "second"]);

    let mut infos = Vec::new();
    loop {
        let (option, reply, payload) = option_reply(&output, &mut cursor);
        assert_eq!(option, NBD_OPT_GO);
        if reply == NBD_REP_ACK {
            break;
        }
        assert_eq!(reply, NBD_REP_INFO);
        let info = u16::from_be_bytes(payload[0..2].try_into().unwrap());
        infos.push(info);
        if info == 3 {
            // BLOCK_SIZE: min/preferred/max are non-zero and ordered.
            let minimum = u32::from_be_bytes(payload[2..6].try_into().unwrap());
            let preferred = u32::from_be_bytes(payload[6..10].try_into().unwrap());
            let maximum = u32::from_be_bytes(payload[10..14].try_into().unwrap());
            assert!(minimum >= 1);
            assert!(preferred >= minimum);
            assert!(maximum >= preferred);
        }
    }
    assert!(infos.contains(&0));
    assert!(infos.contains(&NBD_INFO_NAME));
    assert!(infos.contains(&NBD_INFO_BLOCK_SIZE));
    // GO's ACK ends negotiation; transmission starts directly after it (the
    // 10-byte size/flags response is only used by EXPORT_NAME).
}

#[test]
fn read_bounds_and_read_only_write_rejection() {
    let data = vec![0xAAu8; 4096];
    let catalog = read_only_catalog(data.clone());
    let mut script = handshake_prefix();
    script.extend(export_name_option("media"));
    script.extend(
        NbdRequest::Read {
            handle: 1,
            offset: 4096,
            length: 1,
        }
        .encode(),
    );
    script.extend(
        NbdRequest::Read {
            handle: 2,
            offset: 4090,
            length: 16,
        }
        .encode(),
    );
    script.extend(
        NbdRequest::Read {
            handle: 3,
            offset: 0,
            length: 2048,
        }
        .encode(),
    );
    script.extend(
        NbdRequest::Write {
            handle: 4,
            offset: 0,
            data: vec![0x11; 4],
        }
        .encode(),
    );
    script.extend(NbdRequest::Flush { handle: 5 }.encode());
    script.extend(NbdRequest::Disconnect { handle: 6 }.encode());

    let (mut duplex, output) = Duplex::new(script);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let outcome = serve_session(
        &mut duplex,
        &catalog,
        &configured(1024),
        &ManualClock::new(0),
        &stop,
    )
    .unwrap();
    let output = output.lock().unwrap().clone();
    let mut cursor = 18 + 10;
    for (handle, errno) in [(1, 22), (2, 22), (3, 22), (4, 1), (5, 0)] {
        let (error, replied) = read_reply(&output, &mut cursor);
        assert_eq!(replied, handle);
        assert_eq!(error, errno, "request {handle}");
    }
    // The read-only image behind the served export is unchanged.
    let source = catalog.find("media").unwrap();
    let mut actual = vec![0u8; 4096];
    source.backend.read_at(0, &mut actual).unwrap();
    assert_eq!(actual, vec![0xAAu8; 4096]);
    assert_eq!(outcome.write_requests, 1);
    assert_eq!(outcome.bytes_written, 4);
}

#[test]
fn writable_export_accepts_bounded_writes() {
    let catalog =
        NbdExportCatalog::new(vec![
            NbdExportSource::memory("rw", vec![0u8; 1024], false).unwrap()
        ])
        .unwrap();
    let mut script = handshake_prefix();
    script.extend(export_name_option("rw"));
    script.extend(
        NbdRequest::Write {
            handle: 1,
            offset: 100,
            data: b"ozma".to_vec(),
        }
        .encode(),
    );
    script.extend(
        NbdRequest::Read {
            handle: 2,
            offset: 96,
            length: 12,
        }
        .encode(),
    );
    script.extend(NbdRequest::Disconnect { handle: 3 }.encode());
    let (mut duplex, output) = Duplex::new(script);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let outcome = serve_session(
        &mut duplex,
        &catalog,
        &configured(1024),
        &ManualClock::new(0),
        &stop,
    )
    .unwrap();
    assert_eq!(outcome.write_requests, 1);
    assert_eq!(outcome.bytes_written, 4);
    let output = output.lock().unwrap().clone();
    let mut cursor = 18 + 10;
    assert_eq!(read_reply(&output, &mut cursor).0, 0);
    assert_eq!(read_reply(&output, &mut cursor).0, 0);
    assert_eq!(
        &output[cursor..cursor + 12],
        &[0, 0, 0, 0, b'o', b'z', b'm', b'a', 0, 0, 0, 0]
    );
    // Inspect the backend directly through a second export clone.
    let source = catalog.find("rw").unwrap();
    let mut actual = [0u8; 4];
    source.backend.read_at(100, &mut actual).unwrap();
    assert_eq!(&actual, b"ozma");
}

#[test]
fn malformed_and_unsupported_frames_are_typed() {
    // Bad option magic after a valid banner.
    let catalog = read_only_catalog(vec![0u8; 512]);
    let mut script = handshake_prefix();
    script.extend_from_slice(&[0xFFu8; 16]);
    let (mut duplex, _) = Duplex::new(script);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let error = serve_session(
        &mut duplex,
        &catalog,
        &configured(1024),
        &ManualClock::new(0),
        &stop,
    )
    .unwrap_err();
    assert_eq!(error.kind(), "protocol-error");

    // Bad request magic after a completed negotiation.
    let mut script = handshake_prefix();
    script.extend(export_name_option("media"));
    script.extend_from_slice(&[0u8; NBD_REQUEST_HEADER_LEN]);
    let (mut duplex, _) = Duplex::new(script);
    let error = serve_session(
        &mut duplex,
        &catalog,
        &configured(1024),
        &ManualClock::new(0),
        &stop,
    )
    .unwrap_err();
    assert_eq!(error.kind(), "protocol-error");

    // Unknown option is rejected with ERR_UNSUP and the session stays usable.
    let mut script = handshake_prefix();
    script.extend(option(99, &[1, 2, 3]));
    script.extend(option(NBD_OPT_ABORT, &[]));
    let (mut duplex, output) = Duplex::new(script);
    let outcome = serve_session(
        &mut duplex,
        &catalog,
        &configured(1024),
        &ManualClock::new(0),
        &stop,
    )
    .unwrap();
    assert_eq!(outcome.end, Some(NbdSessionEnd::Aborted));
    let output = output.lock().unwrap().clone();
    let mut cursor = 18;
    let (_, reply, _) = option_reply(&output, &mut cursor);
    assert_eq!(reply, 0x8000_0001);
    let (_, reply, _) = option_reply(&output, &mut cursor);
    assert_eq!(reply, NBD_REP_ACK);
}

#[test]
fn truncated_request_is_a_clean_peer_close() {
    let catalog = read_only_catalog(vec![0u8; 512]);
    let mut script = handshake_prefix();
    script.extend(export_name_option("media"));
    script.extend_from_slice(&[0u8; 8]);
    let (mut duplex, _) = Duplex::new(script);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let outcome = serve_session(
        &mut duplex,
        &catalog,
        &configured(1024),
        &ManualClock::new(0),
        &stop,
    )
    .unwrap();
    assert_eq!(outcome.end, Some(NbdSessionEnd::PeerClosed));
}

struct AdvancingClock(AtomicU64);

impl NbdClock for AdvancingClock {
    fn now_millis(&self) -> u64 {
        self.0.fetch_add(100, Ordering::SeqCst)
    }
}

#[test]
fn injected_clock_enforces_the_handshake_deadline() {
    let catalog = read_only_catalog(vec![0u8; 512]);
    let mut duplex = Duplex::idle(handshake_prefix());
    let clock = AdvancingClock(AtomicU64::new(0));
    let mut config = configured(1024);
    config.handshake_timeout_ms = 250;
    let stop = std::sync::atomic::AtomicBool::new(false);
    let outcome = serve_session(&mut duplex, &catalog, &config, &clock, &stop).unwrap();
    assert_eq!(outcome.end, Some(NbdSessionEnd::Timeout));
}

/// Minimal wire client used only against loopback listeners.
struct WireClient<S: Read + Write> {
    stream: S,
}

impl<S: Read + Write> WireClient<S> {
    fn new(stream: S) -> Self {
        Self { stream }
    }

    fn handshake(&mut self) -> u16 {
        let mut banner = [0u8; 18];
        self.stream.read_exact(&mut banner).unwrap();
        assert_eq!(&banner[..8], NBD_MAGIC);
        assert_eq!(
            u64::from_be_bytes(banner[8..16].try_into().unwrap()),
            NBD_NEWSTYLE_MAGIC
        );
        let flags = u16::from_be_bytes(banner[16..18].try_into().unwrap());
        self.stream
            .write_all(&(NBD_FLAG_C_FIXED_NEWSTYLE | NBD_FLAG_C_NO_ZEROES).to_be_bytes())
            .unwrap();
        flags
    }

    fn select_export(&mut self, name: &str) -> (u64, u16) {
        self.stream.write_all(&export_name_option(name)).unwrap();
        let mut response = [0u8; 10];
        self.stream.read_exact(&mut response).unwrap();
        (
            u64::from_be_bytes(response[0..8].try_into().unwrap()),
            u16::from_be_bytes(response[8..10].try_into().unwrap()),
        )
    }

    fn request(&mut self, request: NbdRequest) {
        self.stream.write_all(&request.encode()).unwrap();
    }

    fn read_exact(&mut self, length: usize) -> Vec<u8> {
        let mut data = vec![0u8; length];
        self.stream.read_exact(&mut data).unwrap();
        data
    }

    fn reply(&mut self) -> (u32, u64) {
        let mut header = [0u8; 16];
        self.stream.read_exact(&mut header).unwrap();
        (
            u32::from_be_bytes(header[4..8].try_into().unwrap()),
            u64::from_be_bytes(header[8..16].try_into().unwrap()),
        )
    }
}

#[test]
fn concurrent_loopback_clients_are_bounded_and_isolated() {
    let data: Vec<u8> = (0..8192u32).map(|value| (value % 251) as u8).collect();
    let catalog = read_only_catalog(data.clone());
    let data = Arc::new(data);
    let server = NbdServer::bind_tcp(
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
        catalog,
        configured(4096),
    )
    .unwrap();
    let address = server.local_addr().unwrap();
    let handle = server.start().unwrap();

    let mut workers = Vec::new();
    for worker in 0..4u64 {
        let data = Arc::clone(&data);
        workers.push(std::thread::spawn(move || {
            let stream = TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut client = WireClient::new(stream);
            client.handshake();
            let (size, flags) = client.select_export("media");
            assert_eq!(size, 8192);
            assert_ne!(flags & NBD_FLAG_READ_ONLY as u16, 0);
            let offset = worker * 512;
            client.request(NbdRequest::Read {
                handle: worker,
                offset,
                length: 512,
            });
            let (error, replied) = client.reply();
            assert_eq!(error, 0);
            assert_eq!(replied, worker);
            let actual = client.read_exact(512);
            assert_eq!(
                actual,
                data[offset as usize..offset as usize + 512].to_vec()
            );
            client.request(NbdRequest::Disconnect {
                handle: 100 + worker,
            });
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }
    // Give the server threads a moment to record the completed sessions.
    for _ in 0..100 {
        if handle.stats().sessions >= 4 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let stats = handle.stop();
    assert!(stats.connections_accepted >= 4);
    assert!(stats.read_requests >= 4);
    assert!(stats.bytes_read >= 4 * 512);
    assert_eq!(stats.connections_rejected, 0);
}

#[test]
fn max_clients_refuses_extra_connections() {
    let catalog = read_only_catalog(vec![0u8; 4096]);
    let mut config = configured(1024);
    config.max_clients = 1;
    let server = NbdServer::bind_tcp(
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
        catalog,
        config,
    )
    .unwrap();
    let address = server.local_addr().unwrap();
    let handle = server.start().unwrap();

    let first = TcpStream::connect(address).unwrap();
    first
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut first = WireClient::new(first);
    first.handshake();
    first.select_export("media");
    for _ in 0..100 {
        if handle.active_clients() == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(handle.active_clients(), 1);

    let mut second = TcpStream::connect(address).unwrap();
    second
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut banner = [0u8; 1];
    // The rejected connection is shut down before a handshake banner.
    assert_eq!(second.read(&mut banner).unwrap_or(0), 0);
    drop(second);

    for _ in 0..100 {
        if handle.stats().connections_rejected >= 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    first.request(NbdRequest::Disconnect { handle: 9 });
    let stats = handle.stop();
    assert!(stats.connections_rejected >= 1);
    assert!(stats.connections_accepted >= 1);
}

#[test]
fn server_stop_wakes_an_idle_client() {
    let catalog = read_only_catalog(vec![0u8; 512]);
    let server = NbdServer::bind_tcp(
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
        catalog,
        configured(1024),
    )
    .unwrap();
    let address = server.local_addr().unwrap();
    let handle = server.start().unwrap();
    let stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    // Wait until the listener has accepted and registered the idle client.
    for _ in 0..100 {
        if handle.active_clients() >= 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(handle.active_clients() >= 1);
    // Do not handshake: the client should be woken by handle.stop().
    let stats = handle.stop();
    assert!(stats.connections_accepted >= 1);
    drop(stream);
}

#[cfg(unix)]
#[test]
fn unix_socket_listener_serves_an_export() {
    use std::os::unix::net::UnixStream;

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("nbd.sock");
    let catalog = read_only_catalog(vec![0x5Au8; 512]);
    let server = NbdServer::bind_unix(&path, catalog, configured(1024)).unwrap();
    let handle = server.start().unwrap();
    let stream = UnixStream::connect(&path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut client = WireClient::new(stream);
    client.handshake();
    let (size, _) = client.select_export("media");
    assert_eq!(size, 512);
    client.request(NbdRequest::Read {
        handle: 1,
        offset: 0,
        length: 8,
    });
    assert_eq!(client.reply(), (0, 1));
    assert_eq!(client.read_exact(8), vec![0x5Au8; 8]);
    client.request(NbdRequest::Disconnect { handle: 2 });
    drop(client);
    let stats = handle.stop();
    assert!(stats.read_requests >= 1);
    assert!(!path.exists(), "Unix socket is removed on stop");
}

#[test]
fn file_and_media_backends_enforce_read_only() {
    use ozma_usb::{build_fat32_image, Fat32Node, MassStorageImage};

    let temp = tempfile::tempdir().unwrap();
    let image = build_fat32_image(vec![Fat32Node::file("hello.txt", b"hello".to_vec())]).unwrap();
    let path = temp.path().join("fat.img");
    std::fs::write(&path, image.as_bytes()).unwrap();
    let media = MassStorageImage::fat32(&path);
    let export = NbdExportSource::from_media("disk", &media).unwrap();
    assert!(export.is_read_only());
    assert_eq!(export.export.size_bytes, image.len() as u64);
    let mut bytes = vec![0u8; 512];
    export.backend.read_at(0, &mut bytes).unwrap();
    assert_eq!(bytes[510..512], [0x55, 0xAA]);
    assert_eq!(
        export.backend.write_at(0, &[0u8; 4]).unwrap_err().kind(),
        "read-only-media"
    );

    let writable_path = temp.path().join("rw.raw");
    std::fs::write(&writable_path, [0u8; 1024]).unwrap();
    let rw = NbdExportSource::file("rw", &writable_path, false).unwrap();
    rw.backend.write_at(8, b"data").unwrap();
    rw.backend.flush().unwrap();
    let mut actual = [0u8; 4];
    rw.backend.read_at(8, &mut actual).unwrap();
    assert_eq!(&actual, b"data");

    // A writable export whose name is not in the catalog cannot be reached.
    let catalog = NbdExportCatalog::new(vec![rw]).unwrap();
    assert!(catalog.find("missing").is_none());
    assert!(catalog.find("").is_some());
}

#[test]
fn configuration_bounds_are_validated() {
    let mut config = NbdServerConfig::default();
    config.max_request_bytes = 0;
    assert_eq!(config.validate().unwrap_err().kind(), "invalid-media");
    config.max_request_bytes = 1024;
    config.max_clients = 0;
    assert_eq!(config.validate().unwrap_err().kind(), "invalid-media");
    config.max_clients = 1;
    config.handshake_timeout_ms = 0;
    assert_eq!(config.validate().unwrap_err().kind(), "invalid-media");
    config.handshake_timeout_ms = 1;
    config.idle_timeout_ms = Some(0);
    assert_eq!(config.validate().unwrap_err().kind(), "invalid-media");
    config.idle_timeout_ms = None;
    config.option_max_bytes = 0;
    assert_eq!(config.validate().unwrap_err().kind(), "invalid-media");
    config.option_max_bytes = 1024;
    config.validate().unwrap();

    let duplicate = NbdExportCatalog::new(vec![
        NbdExportSource::memory("same", vec![0u8; 512], true).unwrap(),
        NbdExportSource::memory("same", vec![0u8; 512], true).unwrap(),
    ]);
    assert_eq!(duplicate.unwrap_err().kind(), "invalid-media");

    let empty = NbdExportCatalog::new(Vec::new()).unwrap_err();
    assert_eq!(empty.kind(), "invalid-media");

    let bad_size = NbdExportSource::from_backend_with_export(
        ozma_usb::NbdExport::read_only("x", 1024),
        Arc::new(NbdMemoryImage::new(vec![0u8; 512], true).unwrap()),
    )
    .unwrap_err();
    assert_eq!(bad_size.kind(), "invalid-media");

    let error: UsbError = NbdExportSource::memory("empty", Vec::new(), true).unwrap_err();
    assert_eq!(error.kind(), "invalid-media");
}
