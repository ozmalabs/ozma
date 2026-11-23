// SPDX-License-Identifier: AGPL-3.0-only
//! Minimal outbound Xpra peer for Ozma scene output.
//!
//! This is deliberately a view-only adapter: it accepts the client's `hello`,
//! advertises a small stable capability set, and sends scene commits as native
//! Xpra window packets. Encoded video is optional and is only forwarded when
//! the client advertised the matching codec. Input, clipboard, server-side
//! desktop control and authentication remain outside this adapter: the peer is
//! loopback-only and accepts the connection without a challenge.
//!
//! Only uncompressed rencodeplus records are accepted. A client that sends a
//! compressed record (typically an LZ4 hello) is closed with a specific
//! protocol error instead of being left waiting.

use std::collections::{BTreeMap, HashMap};
use std::{net::SocketAddr, sync::Arc, time::Duration};

use ozma_protocol::{
    scene::SceneCommit,
    xpra::decode_framed_rencodeplus,
    xpra_output::{
        DrawAck, EncodedRecord, XpraClientEncodings, XpraOutputEncoder, XpraVideoFrame,
        SERVER_ENCODINGS,
    },
};
use serde_json::Value as JsonValue;
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, watch},
    time::timeout,
};

const MAGIC: u8 = b'P';
const RENCODEPLUS: u8 = 0x10;
const MAX_RECORD: usize = 65_535;
const MAX_HELLO: usize = 16 * 1024;
const ACK_TIMEOUT: Duration = Duration::from_secs(5);
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_VERSION: [u32; 2] = [8, 0];

#[derive(Clone, Debug)]
pub struct XpraSceneFrame {
    pub commit: SceneCommit,
    pub resources: BTreeMap<u64, Vec<u8>>,
}

impl XpraSceneFrame {
    /// Extent of the scene placements, used for the server hello `display`
    /// map. A scene with no visible placements reports no size.
    fn display_size(&self) -> Option<(u32, u32)> {
        let mut width = 0u32;
        let mut height = 0u32;
        for image in &self.commit.images {
            if image.x >= 0 {
                width = width.max((image.x as u32).saturating_add(image.width));
            }
            if image.y >= 0 {
                height = height.max((image.y as u32).saturating_add(image.height));
            }
        }
        (width > 0 && height > 0).then_some((width, height))
    }
}

#[derive(Debug, Error)]
pub enum XpraPeerError {
    #[error("Xpra I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("Xpra record is invalid: {0}")]
    Record(String),
    #[error(
        "Xpra record uses unsupported compression {0:#04x}; only uncompressed records are supported"
    )]
    UnsupportedCompression(u8),
    #[error(
        "Xpra record uses protocol flags {0:#04x}; only uncompressed rencodeplus main records are supported"
    )]
    UnsupportedFlags(u8),
    #[error("Xpra raw chunks are not supported")]
    UnsupportedChunk,
    #[error("Xpra record declares {declared} payload bytes, above the {limit} byte limit")]
    RecordTooLarge { declared: usize, limit: usize },
    #[error("Xpra client requires protocol version {client:?}; this server implements {server:?}")]
    UnsupportedVersion { client: [u32; 2], server: [u32; 2] },
    #[error("Xpra client did not advertise PNG")]
    NoPng,
    #[error("Xpra client did not send hello")]
    MissingHello,
    #[error("no scene frame is available")]
    NoFrame,
    #[error("Xpra peer must bind to a loopback address")]
    NonLoopback,
    #[error("scene output: {0}")]
    Output(#[from] ozma_protocol::xpra_output::OutputError),
    #[error("timed out waiting for Xpra window acknowledgement")]
    AckTimeout,
}

pub struct XpraPeerServer {
    listener: TcpListener,
}

impl XpraPeerServer {
    pub async fn bind(addr: SocketAddr) -> Result<Self, XpraPeerError> {
        if !addr.ip().is_loopback() {
            return Err(XpraPeerError::NonLoopback);
        }
        Ok(Self {
            listener: TcpListener::bind(addr).await?,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, XpraPeerError> {
        Ok(self.listener.local_addr()?)
    }

    /// Keep a view-only peer current until its TCP connection closes. Each
    /// commit is serialized in order and draw acknowledgements provide the
    /// backpressure boundary.
    pub async fn serve(
        &self,
        frames: watch::Receiver<Option<Arc<XpraSceneFrame>>>,
    ) -> Result<(), XpraPeerError> {
        self.serve_inner(frames, None).await
    }

    /// Serve a scene plus encoded video frames. The receiver is kept across
    /// reconnect attempts by the caller; frames are discarded when the
    /// connected client did not advertise the matching video codec.
    pub async fn serve_with_video(
        &self,
        frames: watch::Receiver<Option<Arc<XpraSceneFrame>>>,
        video: &mut mpsc::Receiver<XpraVideoFrame>,
    ) -> Result<(), XpraPeerError> {
        self.serve_inner(frames, Some(video)).await
    }

    async fn serve_inner(
        &self,
        mut frames: watch::Receiver<Option<Arc<XpraSceneFrame>>>,
        mut video: Option<&mut mpsc::Receiver<XpraVideoFrame>>,
    ) -> Result<(), XpraPeerError> {
        let (mut stream, _) = self.listener.accept().await?;
        let first = frames
            .borrow_and_update()
            .clone()
            .ok_or(XpraPeerError::NoFrame)?;
        let encodings = handshake(&mut stream, first.display_size()).await?;
        let mut encoder = XpraOutputEncoder::with_encodings(encodings);
        if send_scene_frame(&mut stream, &mut encoder, &first).await? {
            return Ok(());
        }
        write_packet(&mut stream, &[V::Text("startup-complete")]).await?;
        let mut first_byte = [0; 1];
        loop {
            tokio::select! {
                changed = frames.changed() => {
                    if changed.is_err() { return Ok(()); }
                    let frame = frames.borrow_and_update().clone().ok_or(XpraPeerError::NoFrame)?;
                    if send_scene_frame(&mut stream, &mut encoder, &frame).await? {
                        return Ok(());
                    }
                }
                video_frame = async {
                    match video.as_mut() {
                        Some(receiver) => receiver.recv().await.ok_or(()),
                        None => std::future::pending().await,
                    }
                } => {
                    match video_frame {
                        Ok(frame) => {
                            if send_video_frame(&mut stream, &mut encoder, &frame).await? {
                                return Ok(());
                            }
                        }
                        Err(()) => video = None,
                    }
                }
                readable = stream.peek(&mut first_byte) => {
                    if readable? == 0 {
                        return Ok(());
                    }
                    match timeout(ACK_TIMEOUT, read_record(&mut stream, MAX_RECORD)).await {
                        Ok(Ok(packet)) => {
                            if handle_client_packet(&mut stream, &packet).await? {
                                return Ok(());
                            }
                        }
                        Ok(Err(XpraPeerError::Io(error))) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                        Ok(Err(error)) => {
                            close_with(&mut stream, "protocol error").await;
                            return Err(error);
                        }
                        Err(_) => {
                            close_with(&mut stream, "protocol error").await;
                            return Err(XpraPeerError::Record("incomplete client record".into()));
                        }
                    }
                }
            }
        }
    }
}

async fn send_scene_frame(
    stream: &mut TcpStream,
    encoder: &mut XpraOutputEncoder,
    frame: &XpraSceneFrame,
) -> Result<bool, XpraPeerError> {
    if frame.commit.revision <= encoder.revision() {
        return Ok(false);
    }
    let records = encoder.encode_commit(&frame.commit, &frame.resources)?;
    send_records(stream, records).await
}

async fn send_video_frame(
    stream: &mut TcpStream,
    encoder: &mut XpraOutputEncoder,
    frame: &XpraVideoFrame,
) -> Result<bool, XpraPeerError> {
    if !encoder.encodings().supports_video(frame.codec) {
        return Ok(false);
    }
    match encoder.encode_video(frame) {
        Ok(records) => send_records(stream, records).await,
        // A viewer that joined mid-stream, or dropped an access unit, waits for
        // the next keyframe instead of tearing down the connection.
        Err(ozma_protocol::xpra_output::OutputError::VideoKeyframeRequired) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

async fn send_records(
    stream: &mut TcpStream,
    records: Vec<EncodedRecord>,
) -> Result<bool, XpraPeerError> {
    let closed = timeout(ACK_TIMEOUT, async {
        let mut draws: HashMap<i64, u32> = HashMap::new();
        for record in records {
            if let Some(DrawAck {
                window_id,
                sequence,
            }) = record.draw
            {
                draws.insert(sequence, window_id);
            }
            stream.write_all(&record.bytes).await?;
        }
        while !draws.is_empty() {
            let packet = read_record(stream, MAX_RECORD).await?;
            match packet.get(0).and_then(JsonValue::as_str) {
                Some("window-ack") => {
                    let window = packet
                        .get(1)
                        .and_then(JsonValue::as_u64)
                        .and_then(|window| u32::try_from(window).ok());
                    let sequence = packet.get(4).and_then(JsonValue::as_i64);
                    if let (Some(window), Some(sequence)) = (window, sequence) {
                        if draws.get(&sequence).copied() == Some(window) {
                            draws.remove(&sequence);
                        }
                    }
                }
                Some("connection-close") => return Ok(true),
                Some("ping") => answer_ping(stream, &packet).await?,
                _ => {}
            }
        }
        Ok::<_, XpraPeerError>(false)
    })
    .await
    .map_err(|_| XpraPeerError::AckTimeout)??;
    Ok(closed)
}

async fn handle_client_packet(
    stream: &mut TcpStream,
    packet: &JsonValue,
) -> Result<bool, XpraPeerError> {
    match packet.get(0).and_then(JsonValue::as_str) {
        Some("connection-close") => Ok(true),
        Some("ping") => {
            answer_ping(stream, packet).await?;
            Ok(false)
        }
        // Acknowledgements that were not expected, and any packet this
        // view-only adapter does not implement, are ignored.
        _ => Ok(false),
    }
}

async fn answer_ping(stream: &mut TcpStream, packet: &JsonValue) -> Result<(), XpraPeerError> {
    let Some(monotonic) = packet.get(1).and_then(JsonValue::as_i64) else {
        return Ok(());
    };
    let sid = packet.get(3).and_then(JsonValue::as_str);
    if let Some(sid) = sid {
        write_packet(
            stream,
            &[
                V::Text("ping-echo"),
                V::Int(monotonic),
                V::Int(0),
                V::Int(0),
                V::Int(0),
                V::Int(-1),
                V::Text(sid),
            ],
        )
        .await
    } else {
        write_packet(
            stream,
            &[
                V::Text("ping-echo"),
                V::Int(monotonic),
                V::Int(0),
                V::Int(0),
                V::Int(0),
                V::Int(-1),
            ],
        )
        .await
    }
}

async fn handshake(
    stream: &mut TcpStream,
    display_size: Option<(u32, u32)>,
) -> Result<XpraClientEncodings, XpraPeerError> {
    let packet = match timeout(HELLO_TIMEOUT, read_record(stream, MAX_HELLO)).await {
        Ok(Ok(packet)) => packet,
        Ok(Err(error)) => {
            close_with(stream, "protocol error").await;
            return Err(error);
        }
        Err(_) => {
            close_with(stream, "login timeout").await;
            return Err(XpraPeerError::Record("hello timeout".into()));
        }
    };
    if packet.get(0).and_then(JsonValue::as_str) != Some("hello") {
        close_with(stream, "protocol error").await;
        return Err(XpraPeerError::MissingHello);
    }
    if packet.get(1).and_then(JsonValue::as_object).is_none() {
        close_with(stream, "protocol error").await;
        return Err(XpraPeerError::MissingHello);
    }
    if let Err(error) = check_protocol_version(&packet[1]) {
        close_with(stream, "version error").await;
        return Err(error);
    }
    let encodings = XpraClientEncodings::from_hello(&packet[1]);
    if !encodings.png {
        close_with(stream, "protocol error").await;
        return Err(XpraPeerError::NoPng);
    }
    write_server_hello(stream, display_size).await?;
    write_encoding_set(stream).await?;
    Ok(encodings)
}

fn check_protocol_version(caps: &JsonValue) -> Result<(), XpraPeerError> {
    let Some(value) = caps.get("protocol-version") else {
        // A peer that omits the key requires protocol version 7.0.
        return Ok(());
    };
    let parts = value
        .as_array()
        .ok_or_else(|| XpraPeerError::Record("invalid protocol-version".into()))?;
    let major = parts
        .first()
        .and_then(JsonValue::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| XpraPeerError::Record("invalid protocol-version".into()))?;
    let minor = parts
        .get(1)
        .and_then(JsonValue::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0);
    let client = [major, minor];
    if client < [7, 0] || client > SERVER_VERSION {
        return Err(XpraPeerError::UnsupportedVersion {
            client,
            server: SERVER_VERSION,
        });
    }
    Ok(())
}

async fn close_with(stream: &mut TcpStream, reason: &str) {
    let _ = write_packet(stream, &[V::Text("connection-close"), V::Text(reason)]).await;
}

async fn write_server_hello(
    stream: &mut TcpStream,
    display_size: Option<(u32, u32)>,
) -> Result<(), XpraPeerError> {
    let version = [V::Int(8), V::Int(0)];
    let encoders = [V::Text("rencodeplus")];
    let compressors = [V::Text("none")];
    let window = [("enabled", V::Bool(true))];
    let (width, height) = display_size.unwrap_or((1, 1));
    let size = [V::Int(i64::from(width)), V::Int(i64::from(height))];
    let display = [("desktop_size", V::List(&size))];
    let caps = [
        ("version", V::Text("Ozma 0.1")),
        ("protocol-version", V::List(&version)),
        ("encoders", V::List(&encoders)),
        ("compressors", V::List(&compressors)),
        ("chunks", V::Bool(false)),
        ("readonly", V::Bool(true)),
        ("server_type", V::Text("ozma")),
        ("mode", V::Text("seamless")),
        ("session_name", V::Text("Ozma scene output")),
        ("encoding", V::Text("auto")),
        ("window", V::Map(&window)),
        ("display", V::Map(&display)),
    ];
    write_packet(stream, &[V::Text("hello"), V::Map(&caps)]).await
}

async fn write_encoding_set(stream: &mut TcpStream) -> Result<(), XpraPeerError> {
    let all: Vec<V<'_>> = SERVER_ENCODINGS.iter().map(|name| V::Text(name)).collect();
    let core: Vec<V<'_>> = ["h264", "vp8", "vp9", "jpeg", "png"]
        .iter()
        .map(|name| V::Text(name))
        .collect();
    let encodings = [("", V::List(&all)), ("core", V::List(&core))];
    let video = [
        ("h264", V::Map(&[])),
        ("vp8", V::Map(&[])),
        ("vp9", V::Map(&[])),
    ];
    let properties = [("encodings", V::Map(&encodings)), ("video", V::Map(&video))];
    write_packet(stream, &[V::Text("encoding-set"), V::Map(&properties)]).await
}

async fn read_record(stream: &mut TcpStream, limit: usize) -> Result<JsonValue, XpraPeerError> {
    let mut header = [0u8; 8];
    stream.read_exact(&mut header).await?;
    if header[0] != MAGIC {
        return Err(XpraPeerError::Record("invalid magic byte".into()));
    }
    if header[1] & RENCODEPLUS == 0 || header[1] & !0x18 != 0 {
        return Err(XpraPeerError::UnsupportedFlags(header[1]));
    }
    if header[2] != 0 {
        return Err(XpraPeerError::UnsupportedCompression(header[2]));
    }
    if header[3] != 0 {
        return Err(XpraPeerError::UnsupportedChunk);
    }
    let len = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
    if len > limit {
        return Err(XpraPeerError::RecordTooLarge {
            declared: len,
            limit,
        });
    }
    let mut record = header.to_vec();
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload).await?;
    record.append(&mut payload);
    decode_framed_rencodeplus(&record).map_err(|error| XpraPeerError::Record(format!("{error:?}")))
}

async fn write_packet(stream: &mut TcpStream, values: &[V<'_>]) -> Result<(), XpraPeerError> {
    let mut record = vec![MAGIC, RENCODEPLUS, 0, 0, 0, 0, 0, 0];
    encode(V::List(values), &mut record);
    let len = record.len() - 8;
    if len > MAX_RECORD {
        return Err(XpraPeerError::Record("record exceeds limit".into()));
    }
    record[4..8].copy_from_slice(&(len as u32).to_be_bytes());
    stream.write_all(&record).await?;
    Ok(())
}

#[derive(Clone, Copy)]
enum V<'a> {
    Int(i64),
    Bool(bool),
    Text(&'a str),
    Map(&'a [(&'a str, V<'a>)]),
    List(&'a [V<'a>]),
}

fn encode(value: V<'_>, out: &mut Vec<u8>) {
    match value {
        V::Bool(value) => out.push(if value { 67 } else { 68 }),
        V::Int(v) if (0..=43).contains(&v) => out.push(v as u8),
        V::Int(v) => {
            out.push(65);
            out.extend_from_slice(&v.to_be_bytes());
        }
        V::Text(v) => {
            out.extend_from_slice(v.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(v.as_bytes());
        }
        V::Map(entries) => {
            out.push(102 + entries.len() as u8);
            for (k, v) in entries {
                encode(V::Text(k), out);
                encode(*v, out);
            }
        }
        V::List(items) => {
            out.push(192 + items.len() as u8);
            for v in items {
                encode(*v, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_protocol::scene::ImagePlacement;
    use ozma_protocol::xpra_output::{VideoCodec, FULL_DISPLAY_WINDOW_ID};

    fn png() -> Vec<u8> {
        b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR\0\0\0\x01\0\0\0\x01".to_vec()
    }

    fn scene_frame(revision: u64, x: i32) -> XpraSceneFrame {
        XpraSceneFrame {
            commit: SceneCommit {
                revision,
                images: vec![ImagePlacement {
                    node_id: 7,
                    resource_id: 9,
                    x,
                    y: 3,
                    width: 1,
                    height: 1,
                }],
            },
            resources: BTreeMap::from([(9, png())]),
        }
    }

    fn empty_scene() -> XpraSceneFrame {
        XpraSceneFrame {
            commit: SceneCommit {
                revision: 1,
                images: Vec::new(),
            },
            resources: BTreeMap::new(),
        }
    }

    async fn read_until(stream: &mut TcpStream, name: &str) -> JsonValue {
        timeout(Duration::from_secs(2), async {
            loop {
                let packet = read_record(stream, MAX_RECORD).await.unwrap();
                if packet.get(0).and_then(JsonValue::as_str) == Some(name) {
                    return packet;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {name}"))
    }

    async fn ack(stream: &mut TcpStream, window: u32, sequence: i64) {
        write_packet(
            stream,
            &[
                V::Text("window-ack"),
                V::Int(i64::from(window)),
                V::Int(1),
                V::Int(1),
                V::Int(sequence),
                V::Int(0),
                V::Text(""),
            ],
        )
        .await
        .unwrap();
    }

    async fn hello(stream: &mut TcpStream, core: &[V<'_>], csc: bool) {
        let mut encoding: Vec<(&str, V<'_>)> = vec![("core", V::List(core))];
        let option_values = [V::Text("stream")];
        encoding.push(("options", V::List(&option_values)));
        let formats = [V::Text("YUV420P")];
        let full = [("h264", V::List(&formats))];
        if csc {
            encoding.push(("full_csc_modes", V::Map(&full)));
        }
        let caps = [("encoding", V::Map(&encoding))];
        write_packet(stream, &[V::Text("hello"), V::Map(&caps)])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn wire_handshake_sends_scene_and_accepts_ack() {
        let server = XpraPeerServer::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let (tx, rx) = watch::channel(Some(Arc::new(scene_frame(1, 2))));
        let addr = server.local_addr().unwrap();
        let task = tokio::spawn(async move { server.serve(rx).await });
        let mut client = TcpStream::connect(addr).await.unwrap();
        hello(&mut client, &[V::Text("png")], false).await;

        let hello = read_until(&mut client, "hello").await;
        assert_eq!(hello[0], "hello");
        assert_eq!(hello[1]["protocol-version"], serde_json::json!([8, 0]));
        assert_eq!(hello[1]["chunks"], false);
        assert_eq!(hello[1]["readonly"], true);
        assert_eq!(
            hello[1]["display"]["desktop_size"],
            serde_json::json!([3, 4])
        );
        let encoding_set = read_until(&mut client, "encoding-set").await;
        assert_eq!(
            encoding_set[1]["encodings"]["core"],
            serde_json::json!(["h264", "vp8", "vp9", "jpeg", "png"])
        );
        read_until(&mut client, "window-create").await;
        let draw = read_until(&mut client, "window-draw").await;
        assert_eq!(draw[6], "png");
        let sequence = draw[8].as_i64().unwrap();
        ack(&mut client, 7, sequence).await;
        assert_eq!(
            read_until(&mut client, "startup-complete").await[0],
            "startup-complete"
        );

        let mut next = tx.borrow().as_ref().unwrap().as_ref().clone();
        next.commit.revision = 2;
        next.commit.images[0].x = 10;
        tx.send_replace(Some(Arc::new(next)));
        let moved = read_record(&mut client, MAX_RECORD).await.unwrap();
        assert_eq!(
            (moved[0].as_str(), moved[2].as_i64()),
            (Some("window-move-resize"), Some(10))
        );
        assert!(
            timeout(
                Duration::from_millis(300),
                read_record(&mut client, MAX_RECORD)
            )
            .await
            .is_err(),
            "placement-only move must not resend the PNG"
        );
        drop(client);
        assert!(timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .is_ok());
    }

    #[tokio::test]
    async fn wire_video_negotiation_sends_one_encoded_frame() {
        let server = XpraPeerServer::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let (_tx, rx) = watch::channel(Some(Arc::new(empty_scene())));
        let addr = server.local_addr().unwrap();
        let (video_tx, mut video_rx) = mpsc::channel(4);
        let task = tokio::spawn(async move { server.serve_with_video(rx, &mut video_rx).await });
        let mut client = TcpStream::connect(addr).await.unwrap();
        hello(&mut client, &[V::Text("h264"), V::Text("png")], true).await;
        let server_hello = read_until(&mut client, "hello").await;
        assert_eq!(server_hello[0], "hello");
        let encoding_set = read_until(&mut client, "encoding-set").await;
        assert_eq!(
            encoding_set[1]["encodings"]["core"],
            serde_json::json!(["h264", "vp8", "vp9", "jpeg", "png"])
        );
        read_until(&mut client, "startup-complete").await;

        let payload = vec![0, 0, 0, 1, 0x67, 0x42];
        video_tx
            .send(XpraVideoFrame {
                node_id: FULL_DISPLAY_WINDOW_ID,
                codec: VideoCodec::H264,
                width: 64,
                height: 48,
                keyframe: true,
                data: payload.clone(),
            })
            .await
            .unwrap();
        let create = read_until(&mut client, "window-create").await;
        assert_eq!(create[1], u32::MAX);
        let draw = read_until(&mut client, "window-draw").await;
        assert_eq!(draw[1], u32::MAX);
        assert_eq!((draw[4].as_u64(), draw[5].as_u64()), (Some(64), Some(48)));
        assert_eq!(draw[6], "h264");
        assert_eq!(draw[7], serde_json::json!(payload));
        assert_eq!(draw[10]["frame"], 0);
        assert_eq!(draw[10]["type"], "IDR");
        assert_eq!(draw[10]["csc"], "YUV420P");
        ack(&mut client, u32::MAX, draw[8].as_i64().unwrap()).await;
        drop(client);
        assert!(timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .is_ok());
    }

    #[tokio::test]
    async fn png_only_client_receives_no_video() {
        let server = XpraPeerServer::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let (_tx, rx) = watch::channel(Some(Arc::new(empty_scene())));
        let addr = server.local_addr().unwrap();
        let (video_tx, mut video_rx) = mpsc::channel(4);
        let task = tokio::spawn(async move { server.serve_with_video(rx, &mut video_rx).await });
        let mut client = TcpStream::connect(addr).await.unwrap();
        hello(&mut client, &[V::Text("png")], false).await;
        read_until(&mut client, "hello").await;
        read_until(&mut client, "startup-complete").await;

        video_tx
            .send(XpraVideoFrame {
                node_id: FULL_DISPLAY_WINDOW_ID,
                codec: VideoCodec::H264,
                width: 64,
                height: 48,
                keyframe: true,
                data: vec![0, 0, 1],
            })
            .await
            .unwrap();
        assert!(timeout(
            Duration::from_millis(300),
            read_record(&mut client, MAX_RECORD)
        )
        .await
        .is_err());
        drop(client);
        assert!(timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .is_ok());
    }

    #[tokio::test]
    async fn documented_minimal_client_hello_is_accepted() {
        let server = XpraPeerServer::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let (_tx, rx) = watch::channel(Some(Arc::new(empty_scene())));
        let addr = server.local_addr().unwrap();
        let task = tokio::spawn(async move { server.serve(rx).await });
        let mut client = TcpStream::connect(addr).await.unwrap();

        let version = [V::Int(8), V::Int(0)];
        let encoders = [V::Text("rencodeplus")];
        let compressors = [V::Text("lz4"), V::Text("none")];
        let desktop = [V::Int(1920), V::Int(1080)];
        let display = [("desktop_size", V::List(&desktop))];
        let window = [("enabled", V::Bool(true))];
        let core = [V::Text("rgb24"), V::Text("rgb32"), V::Text("png")];
        let options = [V::Text("rgb"), V::Text("png")];
        let rgb_formats = [
            V::Text("BGRX"),
            V::Text("BGRA"),
            V::Text("RGBX"),
            V::Text("RGBA"),
        ];
        let encoding = [
            ("options", V::List(&options)),
            ("core", V::List(&core)),
            ("rgb_formats", V::List(&rgb_formats)),
            ("rgb_lz4", V::Bool(true)),
            ("transparency", V::Bool(false)),
        ];
        let keymap = [("layout", V::Text("us"))];
        let cursor_formats = [V::Text("raw"), V::Text("png")];
        let cursor = [("encodings", V::List(&cursor_formats))];
        let double_click = [V::Int(400), V::Int(5)];
        let pointer = [("double_click", V::List(&double_click))];
        let caps = [
            ("version", V::Text("1.0")),
            ("protocol-version", V::List(&version)),
            ("encoders", V::List(&encoders)),
            ("compressors", V::List(&compressors)),
            ("compression_level", V::Int(0)),
            ("uuid", V::Text("1b4e28ba2fa1411d8f2c8b6b0e2a2f3c")),
            ("username", V::Text("user")),
            ("ping", V::Bool(true)),
            ("server-features", V::Bool(true)),
            ("display", V::Map(&display)),
            ("window", V::Map(&window)),
            ("encoding", V::Map(&encoding)),
            ("keyboard", V::Bool(true)),
            ("keymap", V::Map(&keymap)),
            ("pointer", V::Map(&pointer)),
            ("cursor", V::Map(&cursor)),
            ("unknown-future-capability", V::Map(&[])),
        ];
        write_packet(&mut client, &[V::Text("hello"), V::Map(&caps)])
            .await
            .unwrap();
        let server_hello = read_until(&mut client, "hello").await;
        assert_eq!(
            server_hello[1]["protocol-version"],
            serde_json::json!([8, 0])
        );
        assert_eq!(
            server_hello[1]["encoders"],
            serde_json::json!(["rencodeplus"])
        );
        read_until(&mut client, "startup-complete").await;
        drop(client);
        assert!(timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .is_ok());
    }

    #[tokio::test]
    async fn unknown_client_record_is_ignored_and_close_is_honoured() {
        let server = XpraPeerServer::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let (_tx, rx) = watch::channel(Some(Arc::new(empty_scene())));
        let addr = server.local_addr().unwrap();
        let task = tokio::spawn(async move { server.serve(rx).await });
        let mut client = TcpStream::connect(addr).await.unwrap();
        hello(&mut client, &[V::Text("png")], false).await;
        read_until(&mut client, "hello").await;
        read_until(&mut client, "startup-complete").await;

        let nested = [("future", V::Int(2))];
        write_packet(
            &mut client,
            &[
                V::Text("future-packet"),
                V::Map(&nested),
                V::List(&[V::Int(1), V::Int(2)]),
            ],
        )
        .await
        .unwrap();
        write_packet(
            &mut client,
            &[V::Text("connection-close"), V::Text("client exit")],
        )
        .await
        .unwrap();
        assert!(timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .is_ok());
    }

    #[tokio::test]
    async fn compressed_client_record_is_rejected_without_hanging() {
        let server = XpraPeerServer::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let (_tx, rx) = watch::channel(Some(Arc::new(empty_scene())));
        let addr = server.local_addr().unwrap();
        let task = tokio::spawn(async move { server.serve(rx).await });
        let mut client = TcpStream::connect(addr).await.unwrap();
        let mut record = vec![MAGIC, RENCODEPLUS, 0x11, 0, 0, 0, 0, 4];
        record.extend_from_slice(&[0, 0, 0, 0]);
        client.write_all(&record).await.unwrap();
        let result = timeout(Duration::from_secs(2), task)
            .await
            .expect("server did not reject compressed record")
            .unwrap();
        assert!(matches!(
            result,
            Err(XpraPeerError::UnsupportedCompression(0x11))
        ));
    }

    #[tokio::test]
    async fn compressed_client_record_after_handshake_is_rejected() {
        let server = XpraPeerServer::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let (_tx, rx) = watch::channel(Some(Arc::new(empty_scene())));
        let addr = server.local_addr().unwrap();
        let task = tokio::spawn(async move { server.serve(rx).await });
        let mut client = TcpStream::connect(addr).await.unwrap();
        hello(&mut client, &[V::Text("png")], false).await;
        read_until(&mut client, "hello").await;
        read_until(&mut client, "startup-complete").await;

        let mut record = vec![MAGIC, RENCODEPLUS, 0x11, 0, 0, 0, 0, 4];
        record.extend_from_slice(&[0, 0, 0, 0]);
        client.write_all(&record).await.unwrap();
        let result = timeout(Duration::from_secs(2), task)
            .await
            .expect("server did not reject compressed record")
            .unwrap();
        assert!(matches!(
            result,
            Err(XpraPeerError::UnsupportedCompression(0x11))
        ));
    }

    #[tokio::test]
    async fn rejects_protocol_version_newer_than_server() {
        let server = XpraPeerServer::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let (_tx, rx) = watch::channel(Some(Arc::new(empty_scene())));
        let addr = server.local_addr().unwrap();
        let task = tokio::spawn(async move { server.serve(rx).await });
        let mut client = TcpStream::connect(addr).await.unwrap();
        let version = [V::Int(9), V::Int(0)];
        let caps = [("protocol-version", V::List(&version))];
        write_packet(&mut client, &[V::Text("hello"), V::Map(&caps)])
            .await
            .unwrap();
        let result = timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            result,
            Err(XpraPeerError::UnsupportedVersion {
                client: [9, 0],
                server: [8, 0]
            })
        ));
    }
}
