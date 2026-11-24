// SPDX-License-Identifier: AGPL-3.0-only
//! Integration tests for the QMP `screendump` capture provider.
//!
//! A fake QMP server runs on a temporary Unix socket and writes a real P6 PPM
//! file to the filename QEMU would receive. No QEMU, hypervisor, fixed socket
//! path, or network port is required.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ozma_capture::qmp_display::{QmpDisplayConfig, QmpDisplayProvider, QmpDisplaySource};
use ozma_capture::{CaptureKind, CaptureProvider, CaptureRegistry};
use ozma_media::{CaptureSource, FrameFormat, MediaError};
use ozma_protocol::codes;
use ozma_qmp::{QmpConfig, SharedQmpClient};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

const GREETING: &[u8] = br#"{"QMP":{"version":{"qemu":{"major":8,"minor":0,"micro":0},"package":"ozma-fake"},"capabilities":[]}}"#;

const PIXELS_2X2: [u8; 12] = [
    0, 1, 2, 3, 4, 5, //
    6, 7, 8, 9, 10, 11,
];

fn ppm_2x2() -> Vec<u8> {
    let mut data = b"P6\n2 2\n255\n".to_vec();
    data.extend_from_slice(&PIXELS_2X2);
    data
}

/// A fake QMP control socket that answers `screendump` by writing `ppm`.
struct FakeScreendumpServer {
    _dir: TempDir,
    socket: PathBuf,
    handle: JoinHandle<()>,
    screendumps: Arc<AtomicUsize>,
    connections: Arc<AtomicUsize>,
}

impl FakeScreendumpServer {
    async fn spawn(ppm: Vec<u8>, fail_screendump: bool) -> Self {
        let directory = tempfile::tempdir().expect("tempdir for fake QMP server");
        let socket = directory.path().join("qmp.sock");
        let listener = UnixListener::bind(&socket).expect("bind fake QMP socket");
        let screendumps = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&screendumps);
        let connections = Arc::new(AtomicUsize::new(0));
        let accepted = Arc::clone(&connections);

        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let index = accepted.fetch_add(1, Ordering::SeqCst);
                if index > 0 {
                    // A second control client is a test failure; dropping the
                    // stream makes it visible as a closed handshake.
                    drop(stream);
                    continue;
                }
                let ppm = ppm.clone();
                let count = Arc::clone(&count);
                tokio::spawn(async move {
                    if let Err(error) = serve(stream, &ppm, fail_screendump, &count).await {
                        eprintln!("fake QMP screendump server failed: {error}");
                    }
                });
            }
        });

        Self {
            _dir: directory,
            socket,
            handle,
            screendumps,
            connections,
        }
    }

    fn socket(&self) -> &PathBuf {
        &self.socket
    }

    fn screendumps(&self) -> usize {
        self.screendumps.load(Ordering::Relaxed)
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

impl Drop for FakeScreendumpServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn serve(
    stream: UnixStream,
    ppm: &[u8],
    fail_screendump: bool,
    screendumps: &AtomicUsize,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    write_half.write_all(GREETING).await?;
    write_half.write_all(b"\n").await?;

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let command: Value =
            serde_json::from_str(line.trim()).expect("client sent invalid JSON to fake QMP server");

        match command["execute"].as_str().unwrap_or_default() {
            "qmp_capabilities" => {
                write_half.write_all(b"{\"return\":{}}\n").await?;
            }
            "screendump" => {
                if fail_screendump {
                    write_half
                        .write_all(
                            b"{\"error\":{\"class\":\"GenericError\",\"desc\":\"injected screendump failure\"}}\n",
                        )
                        .await?;
                } else {
                    let filename = command["arguments"]["filename"]
                        .as_str()
                        .expect("screendump filename");
                    tokio::fs::write(filename, ppm).await?;
                    screendumps.fetch_add(1, Ordering::Relaxed);
                    write_half.write_all(b"{\"return\":{}}\n").await?;
                }
            }
            "query-status" => {
                write_half
                    .write_all(b"{\"return\":{\"status\":\"running\",\"running\":true}}\n")
                    .await?;
            }
            other => {
                let response = json!({
                    "error": {
                        "class": "CommandNotFound",
                        "desc": format!("unknown command {other}"),
                    }
                });
                write_half
                    .write_all(&serde_json::to_vec(&response).expect("encode fake response"))
                    .await?;
                write_half.write_all(b"\n").await?;
            }
        }
        write_half.flush().await?;
    }
}

#[test]
fn enumerate_requires_an_existing_socket_path() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("qmp.sock");
    let provider = QmpDisplayProvider::new(QmpDisplayConfig::new(socket.as_path()));

    assert!(provider.enumerate().unwrap().is_empty());

    // Discovery must not connect; a plain file is enough to make the path exist.
    std::fs::write(&socket, b"placeholder").unwrap();
    let capabilities = provider.enumerate().unwrap();
    assert_eq!(capabilities.len(), 1);

    let capability = &capabilities[0];
    assert_eq!(capability.id, "qmp0");
    assert_eq!(capability.name, "QEMU VM display");
    assert_eq!(capability.kind, CaptureKind::Qmp);
    assert_eq!(capability.formats, ["rgb24"]);
    assert_eq!((capability.max_width, capability.max_height), (1920, 1080));
    assert_eq!(capability.max_fps, 30);
    assert!(!capability.compressed);
    assert!(!capability.hdr);
    assert!(!capability.has_audio);
}

#[test]
fn registry_registers_qmp_only_when_asked() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("qmp.sock");
    std::fs::write(&socket, b"placeholder").unwrap();

    let registry = CaptureRegistry::new().with_qmp_display(QmpDisplayConfig::new(socket.as_path()));
    assert_eq!(registry.provider_count(), 1);

    let scans = registry.scan();
    assert_eq!(scans[0].kind, CaptureKind::Qmp);
    assert_eq!(scans[0].capabilities.len(), 1);

    // The hardware/system registries stay QMP-free.
    assert!(CaptureRegistry::system()
        .scan()
        .iter()
        .all(|scan| scan.kind != CaptureKind::Qmp));
}

#[tokio::test]
async fn screendump_produces_rgb24_frames_with_increasing_sequence_and_pts() {
    let server = FakeScreendumpServer::spawn(ppm_2x2(), false).await;
    let output = tempfile::tempdir().unwrap();

    let config = QmpDisplayConfig::new(server.socket().to_path_buf())
        .with_max_fps(240)
        .with_screendump_dir(output.path());
    let mut source = QmpDisplaySource::new(config).unwrap();

    let first = tokio::time::timeout(Duration::from_secs(5), source.next_frame())
        .await
        .expect("first frame timed out")
        .expect("first frame failed");
    assert_eq!(first.format(), FrameFormat::rgb24(2, 2));
    assert_eq!(first.data(), &PIXELS_2X2[..]);
    assert_eq!(first.sequence(), 0);
    assert!(!first.is_keyframe());
    assert_eq!(source.format(), FrameFormat::rgb24(2, 2));

    let second = tokio::time::timeout(Duration::from_secs(5), source.next_frame())
        .await
        .expect("second frame timed out")
        .expect("second frame failed");
    assert_eq!(second.data(), &PIXELS_2X2[..]);
    assert_eq!(second.sequence(), 1);
    assert!(second.pts_micros() > first.pts_micros());
    assert_eq!(server.screendumps(), 2);

    // Each PPM is removed after it has been read.
    let leftovers: Vec<_> = std::fs::read_dir(output.path()).unwrap().collect();
    assert!(leftovers.is_empty(), "screendump files were not cleaned up");

    assert!(!source.is_stopped());
    source.stop().await.unwrap();
    assert!(source.is_stopped());
    assert!(!source.is_connected());
    source.stop().await.unwrap();
    assert!(matches!(
        source.next_frame().await,
        Err(MediaError::EndOfStream)
    ));
}

#[tokio::test]
async fn shared_client_source_uses_one_qmp_connection() {
    let server = FakeScreendumpServer::spawn(ppm_2x2(), false).await;
    let output = tempfile::tempdir().unwrap();
    let config = QmpDisplayConfig::new(server.socket().to_path_buf())
        .with_max_fps(240)
        .with_screendump_dir(output.path());

    let shared = SharedQmpClient::connect(QmpConfig::new(server.socket().to_path_buf()))
        .await
        .unwrap();
    assert_eq!(server.connections(), 1);

    // Provider enumerate is unchanged by sharing; opening must not connect.
    let registry = CaptureRegistry::new().with_qmp_display_shared(config.clone(), shared.clone());
    let capabilities = registry.enumerate().unwrap();
    assert_eq!(capabilities.len(), 1);
    assert_eq!(server.connections(), 1);

    let mut source = registry.open(&capabilities[0]).unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), source.next_frame())
        .await
        .expect("shared source frame timed out")
        .expect("shared source frame failed");
    assert_eq!(frame.format(), FrameFormat::rgb24(2, 2));
    assert_eq!(frame.data(), &PIXELS_2X2[..]);
    assert_eq!(server.screendumps(), 1);

    // Another role can use the same client between frames.
    assert!(shared.query_status().await.unwrap().running);

    source.stop().await.unwrap();
    // Stopping a shared source must not disconnect the node's connection.
    assert!(shared.is_connected().await);
    assert_eq!(server.connections(), 1);
}

#[tokio::test]
async fn qmp_screendump_error_is_a_typed_media_error() {
    let server = FakeScreendumpServer::spawn(ppm_2x2(), true).await;
    let output = tempfile::tempdir().unwrap();
    let config =
        QmpDisplayConfig::new(server.socket().to_path_buf()).with_screendump_dir(output.path());
    let mut source = QmpDisplaySource::new(config).unwrap();

    let error = tokio::time::timeout(Duration::from_secs(5), source.next_frame())
        .await
        .expect("screendump error timed out")
        .expect_err("screendump should fail");

    assert!(matches!(error, MediaError::Capture { .. }), "{error:?}");
    let api = error.to_api_error();
    assert_eq!(api.code, codes::CAPTURE_UNAVAILABLE);
    assert!(api.retryable);

    // The failed read must not leave the temporary path behind.
    let leftovers: Vec<_> = std::fs::read_dir(output.path()).unwrap().collect();
    assert!(leftovers.is_empty(), "failed screendump left a file behind");
}

#[tokio::test]
async fn missing_socket_is_a_typed_media_error() {
    let directory = tempfile::tempdir().unwrap();
    let config = QmpDisplayConfig::new(directory.path().join("absent.sock"));
    let mut source = QmpDisplaySource::new(config).unwrap();

    let error = tokio::time::timeout(Duration::from_secs(5), source.next_frame())
        .await
        .expect("connect attempt timed out")
        .expect_err("a missing socket must fail");

    assert!(matches!(error, MediaError::Capture { .. }), "{error:?}");
    let api = error.to_api_error();
    assert_eq!(api.code, codes::CAPTURE_UNAVAILABLE);
    assert!(api.retryable);
    assert!(!source.is_connected());
}
