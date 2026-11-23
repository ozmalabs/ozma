//! Minimal outbound Xpra peer for Ozma scene output.
//!
//! This is deliberately a view-only adapter: it accepts the client's `hello`,
//! advertises a small stable capability set, and sends scene commits as native
//! Xpra window packets.  Input, clipboard, authentication and server-side
//! desktop control remain outside this adapter.

use std::{collections::BTreeMap, net::SocketAddr, sync::Arc, time::Duration};

use ozma_protocol::{scene::SceneCommit, xpra::decode_framed_rencodeplus, xpra_output::XpraOutputEncoder};
use serde_json::Value as JsonValue;
use thiserror::Error;
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::{TcpListener, TcpStream}, sync::watch, time::timeout};

const MAGIC: u8 = b'P';
const RENCODEPLUS: u8 = 0x10;
const MAX_RECORD: usize = 65_535;
const ACK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct XpraSceneFrame {
    pub commit: SceneCommit,
    pub resources: BTreeMap<u64, Vec<u8>>,
}

#[derive(Debug, Error)]
pub enum XpraPeerError {
    #[error("Xpra I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("Xpra record is invalid: {0}")]
    Record(String),
    #[error("Xpra client did not send hello")]
    MissingHello,
    #[error("no scene frame is available")]
    NoFrame,
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
        Ok(Self { listener: TcpListener::bind(addr).await? })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, XpraPeerError> { Ok(self.listener.local_addr()?) }

    /// Accept one viewer, perform the lightweight handshake, send one frame,
    /// and wait for all `window-draw` acknowledgements. This small operation is
    /// useful to embed in a larger session loop and makes backpressure explicit.
    pub async fn serve_once(
        &self,
        frames: watch::Receiver<Option<Arc<XpraSceneFrame>>>,
    ) -> Result<(), XpraPeerError> {
        let (mut stream, _) = self.listener.accept().await?;
        handshake(&mut stream).await?;
        let frame = frames.borrow().clone().ok_or(XpraPeerError::NoFrame)?;
        let mut encoder = XpraOutputEncoder::new();
        let records = encoder.encode_commit(&frame.commit, &frame.resources)?;
        let mut draws = Vec::new();
        for record in records {
            if is_draw(&record) { draws.push(draw_sequence(&record)?); }
            stream.write_all(&record).await?;
        }
        for sequence in draws { wait_for_ack(&mut stream, sequence).await?; }
        Ok(())
    }
}

async fn handshake(stream: &mut TcpStream) -> Result<(), XpraPeerError> {
    let packet = read_record(stream).await?;
    let is_hello = packet.get(0).and_then(JsonValue::as_str) == Some("hello");
    if !is_hello { return Err(XpraPeerError::MissingHello); }
    let caps = [
        ("version", V::Text("5.0")),
        ("server_type", V::Text("ozma")),
        ("display", V::Text(":0")),
    ];
    write_packet(stream, &[V::Text("hello"), V::Map(&caps)]).await
}

async fn wait_for_ack(stream: &mut TcpStream, sequence: i64) -> Result<(), XpraPeerError> {
    let result = timeout(ACK_TIMEOUT, async {
        loop {
            let packet = read_record(stream).await?;
            if packet.get(0).and_then(JsonValue::as_str) != Some("window-ack") { continue; }
            if packet.get(4).and_then(JsonValue::as_i64) == Some(sequence) { return Ok(()); }
        }
    }).await;
    match result {
        Ok(value) => value,
        Err(_) => Err(XpraPeerError::AckTimeout),
    }
}

fn is_draw(record: &[u8]) -> bool {
    decode_framed_rencodeplus(record).ok().and_then(|v| v.get(0).and_then(JsonValue::as_str).map(|s| s == "window-draw")).unwrap_or(false)
}

fn draw_sequence(record: &[u8]) -> Result<i64, XpraPeerError> {
    decode_framed_rencodeplus(record).map_err(|e| XpraPeerError::Record(e.to_string()))?.get(8).and_then(JsonValue::as_i64).ok_or_else(|| XpraPeerError::Record("window-draw has no sequence".into()))
}

async fn read_record(stream: &mut TcpStream) -> Result<JsonValue, XpraPeerError> {
    let mut header = [0u8; 8];
    stream.read_exact(&mut header).await?;
    if header[0] != MAGIC || header[1] != RENCODEPLUS { return Err(XpraPeerError::Record("unsupported Xpra header".into())); }
    let len = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
    if len > MAX_RECORD { return Err(XpraPeerError::Record("record exceeds limit".into())); }
    let mut record = header.to_vec();
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload).await?;
    record.append(&mut payload);
    decode_framed_rencodeplus(&record).map_err(|e| XpraPeerError::Record(e.to_string()))
}

async fn write_packet(stream: &mut TcpStream, values: &[V<'_>]) -> Result<(), XpraPeerError> {
    let mut record = vec![MAGIC, RENCODEPLUS, 0, 0, 0, 0, 0, 0];
    encode(V::List(values), &mut record);
    let len = record.len() - 8;
    if len > MAX_RECORD { return Err(XpraPeerError::Record("record exceeds limit".into())); }
    record[4..8].copy_from_slice(&(len as u32).to_be_bytes());
    stream.write_all(&record).await?;
    Ok(())
}

enum V<'a> { Text(&'a str), Map(&'a [(&'a str, V<'a>)]), List(&'a [V<'a>]) }
fn encode(value: V<'_>, out: &mut Vec<u8>) {
    match value {
        V::Text(v) => { out.extend_from_slice(v.len().to_string().as_bytes()); out.push(b':'); out.extend_from_slice(v.as_bytes()); }
        V::Map(entries) => { out.push(102 + entries.len() as u8); for (k, v) in entries { encode(V::Text(k), out); encode(v, out); } }
        V::List(items) => { out.push(192 + items.len() as u8); for v in items { encode(v, out); } }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_protocol::scene::ImagePlacement;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn wire_handshake_sends_scene_and_accepts_ack() {
        let server = XpraPeerServer::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let (tx, rx) = watch::channel(Some(Arc::new(XpraSceneFrame {
            commit: SceneCommit { revision: 1, images: vec![ImagePlacement { node_id: 7, resource_id: 9, x: 2, y: 3, width: 1, height: 1 }] },
            resources: BTreeMap::from([(9, b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR\0\0\0\x01\0\0\0\x01".to_vec())]),
        })));
        let addr = server.local_addr().unwrap();
        let task = tokio::spawn(async move { server.serve_once(rx).await });
        let mut client = TcpStream::connect(addr).await.unwrap();
        write_packet(&mut client, &[V::Text("hello"), V::Map(&[])]).await.unwrap();
        assert_eq!(read_record(&mut client).await.unwrap().get(0).unwrap(), "hello");
        let mut sequence = None;
        for _ in 0..2 {
            let packet = read_record(&mut client).await.unwrap();
            if packet.get(0).and_then(JsonValue::as_str) == Some("window-draw") { sequence = packet.get(8).and_then(JsonValue::as_i64); }
        }
        let seq = sequence.unwrap();
        write_packet(&mut client, &[V::Text("window-ack"), V::Text("7"), V::Text("1"), V::Text("1"), V::Text(&seq.to_string())]).await.err();
        // ACK values are numeric in real rencodeplus; use a direct fixture below.
        let mut ack = vec![MAGIC, RENCODEPLUS, 0, 0, 0, 0, 0, 0];
        encode(V::List(&[V::Text("window-ack"), V::Text("7"), V::Text("1"), V::Text("1"), V::Text("1")]), &mut ack);
        let len = ack.len() - 8; ack[4..8].copy_from_slice(&(len as u32).to_be_bytes()); client.write_all(&ack).await.unwrap();
        let _ = tx;
        assert!(task.await.unwrap().is_ok());
    }
}
