// SPDX-License-Identifier: AGPL-3.0-only
//! Browser WebTransport adapter for Ozma sessions.
//!
//! The node exposes a WebTransport endpoint that carries the same channel
//! framing as the native QUIC adapter: one reliable bidirectional stream for
//! ordered channels, datagrams for unreliable channels, and unidirectional
//! streams for resources. This crate is only built for
//! `wasm32-unknown-unknown`; it wraps the browser's `WebTransport` object and
//! implements [`ozma_client_core::ChannelTransport`].
//!
//! # Handshake
//!
//! [`BrowserTransport`] is a byte pipe only. The caller layers
//! [`ozma_client_core::ClientDriver`] on top, which sends
//! `ControlMessage::Hello` as the first reliable control frame and waits for
//! `HelloAck`, exactly like `ozma-node/src/transport.rs::read_hello`.
//!
//! # Certificate pinning
//!
//! Self-signed node certificates must be pinned with
//! [`BrowserConnectOptions::certificate_sha256`]. Browsers require the pinned
//! certificate to be ECDSA with a validity of at most 14 days; a mismatch
//! surfaces as [`WebTransportError::Certificate`].
//!
//! # Building
//!
//! `WebTransport`, `WebTransportOptions`, `WebTransportHash`, and the
//! `VideoDecoder`/`AudioDecoder` APIs are marked unstable by `web-sys`, so the
//! wasm check/build must enable them explicitly:
//!
//! ```text
//! RUSTFLAGS="--cfg=web_sys_unstable_apis" \
//!     cargo check -p ozma-transport-web --target wasm32-unknown-unknown
//! ```
//!
//! Do not put this in a workspace-wide `.cargo/config.toml`: it would affect
//! native crates. The flag belongs on the wasm build command (or in the
//! browser client's build script).
//!
//! # Certificate hashing
//!
//! [`certificate_sha256_der`] and [`certificate_sha256_from_base64`] convert
//! the node's `/api/v1/transport` `certificate_der_base64` into the 32-byte
//! pin the browser needs. Keeping the helper here means `ozma-web` does not
//! need its own `sha2`/`base64` dependency.
//!
//! # Timeouts and the event loop
//!
//! Every network operation runs in a `spawn_local` task and reports through
//! typed events, so [`ozma_client_core::ChannelTransport::recv`] never blocks
//! the browser event loop. The adapter owns no timer; connect/handshake
//! timeouts belong to the caller (`AbortSignal`, `setTimeout`, or a
//! `wasm-bindgen-futures` select).

#![cfg(target_arch = "wasm32")]
#![forbid(unsafe_code)]

use std::cell::Cell;

use async_trait::async_trait;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use bytes::Bytes;
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use ozma_client_core::framing::{encode_datagram, encode_reliable, FrameDecoder};
use ozma_client_core::session::codes as client_codes;
use ozma_client_core::ChannelTransport;
use ozma_protocol::{ApiError, ChannelKind, TransportDescriptor, TransportMode};
use ozma_transport::{ChannelId, ChannelMap, CloseReason, TransportEvent};
use sha2::{Digest, Sha256};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{
    ReadableStream, ReadableStreamDefaultReader, WebTransport, WebTransportBidirectionalStream,
    WebTransportCloseInfo, WebTransportDatagramDuplexStream, WebTransportHash, WebTransportOptions,
    WritableStream, WritableStreamDefaultWriter,
};

/// Default cap for one reliable frame payload, matching the protocol's
/// `u16::MAX` channel limit.
pub const DEFAULT_MAX_PAYLOAD: usize = u16::MAX as usize;

/// Everything [`BrowserTransport::connect`] needs.
#[derive(Clone, Debug)]
pub struct BrowserConnectOptions {
    /// `https://` WebTransport URL from the session offer.
    pub url: String,
    /// SHA-256 of the node's DER certificate, for `serverCertificateHashes`.
    pub certificate_sha256: [u8; 32],
    /// Channels negotiated in the offer's transport descriptor.
    pub channel_map: ChannelMap,
    /// Maximum reliable frame payload accepted or sent. Datagrams are also
    /// capped by the browser's `maxDatagramSize`.
    pub max_payload: usize,
}

impl BrowserConnectOptions {
    /// Build options for an `https://` URL and a pinned certificate hash.
    pub fn new(url: impl Into<String>, certificate_sha256: [u8; 32]) -> Self {
        Self {
            url: url.into(),
            certificate_sha256,
            channel_map: ChannelMap::default_session(),
            max_payload: DEFAULT_MAX_PAYLOAD,
        }
    }

    /// Use the offer's negotiated channel map.
    pub fn with_channel_map(mut self, channel_map: ChannelMap) -> Self {
        self.channel_map = channel_map;
        self
    }

    /// Override the reliable frame payload limit.
    pub fn with_max_payload(mut self, max_payload: usize) -> Self {
        self.max_payload = max_payload.max(1);
        self
    }

    /// Build options directly from the offer's WebTransport descriptor.
    ///
    /// The descriptor must be [`TransportMode::Quic`] with an `https://`
    /// endpoint and a hex SHA-256 `certificate_fingerprint`. An empty channel
    /// list falls back to the standard session layout.
    pub fn from_descriptor(descriptor: &TransportDescriptor) -> Result<Self, WebTransportError> {
        let url = descriptor
            .endpoint
            .as_deref()
            .filter(|endpoint| endpoint.starts_with("https://"))
            .ok_or_else(|| {
                WebTransportError::Unsupported(
                    "the transport descriptor has no https:// endpoint".into(),
                )
            })?;
        let fingerprint = descriptor
            .certificate_fingerprint
            .as_deref()
            .ok_or_else(|| {
                WebTransportError::Certificate(
                    "the transport descriptor has no certificate fingerprint".into(),
                )
            })?;
        let certificate_sha256 = certificate_sha256_from_fingerprint(fingerprint)?;
        let channel_map = ozma_client_core::channel_map_for(descriptor)
            .map_err(|error| WebTransportError::Protocol(error.message))?;
        Ok(Self {
            url: url.to_string(),
            certificate_sha256,
            channel_map,
            max_payload: DEFAULT_MAX_PAYLOAD,
        })
    }

    /// True when this descriptor looks like a browser WebTransport path.
    pub fn matches(descriptor: &TransportDescriptor) -> bool {
        descriptor.mode == TransportMode::Quic
            && descriptor
                .endpoint
                .as_deref()
                .is_some_and(|endpoint| endpoint.starts_with("https://"))
    }
}

/// A typed browser transport failure.
#[derive(Debug, Clone, thiserror::Error)]
pub enum WebTransportError {
    /// WebTransport is unavailable, the URL is not `https://`, or the
    /// connection cannot carry datagrams.
    #[error("WebTransport is not available: {0}")]
    Unsupported(String),
    /// The connection or handshake failed.
    #[error("WebTransport connection failed: {0}")]
    Connect(String),
    /// Certificate pinning or validation failed.
    #[error("WebTransport certificate pinning failed: {0}")]
    Certificate(String),
    /// The transport is closed.
    #[error("WebTransport is closed: {0}")]
    Closed(String),
    /// A stream or datagram operation failed.
    #[error("WebTransport I/O error: {0}")]
    Io(String),
    /// Framing or channel negotiation was violated.
    #[error("WebTransport protocol error: {0}")]
    Protocol(String),
    /// A payload exceeded the negotiated or browser limit.
    #[error("payload of {len} bytes exceeds the {max}-byte limit on channel {channel}")]
    PayloadTooLarge { channel: u8, len: usize, max: usize },
}

impl WebTransportError {
    /// Convert to the shared client error vocabulary.
    pub fn into_api_error(self) -> ApiError {
        match self {
            Self::Unsupported(message) => ApiError::new(client_codes::CLIENT_UNSUPPORTED, message)
                .with_remediation(
                    "Use a browser with WebTransport datagram support and an https:// node URL.",
                ),
            Self::Certificate(message) => ApiError::new(client_codes::CLIENT_CERTIFICATE, message)
                .with_remediation(
                    "Fetch /api/v1/transport and pin the certificate SHA-256 the node advertises.",
                ),
            Self::Connect(message) | Self::Io(message) => {
                ApiError::new(ozma_protocol::codes::TRANSPORT_UNAVAILABLE, message).retryable()
            }
            Self::Closed(message) => ApiError::new(client_codes::CLIENT_DISCONNECTED, message)
                .with_remediation("Reconnect the session and retry."),
            Self::Protocol(message) => ApiError::new(client_codes::CLIENT_PROTOCOL, message),
            Self::PayloadTooLarge { channel, len, max } => {
                ozma_transport::payload_too_large(ChannelId::new(channel), len, max)
            }
        }
    }
}

impl From<WebTransportError> for ApiError {
    fn from(error: WebTransportError) -> Self {
        error.into_api_error()
    }
}

/// A connected browser WebTransport session.
///
/// The transport owns four background tasks: the reliable write serializer,
/// the reliable stream reader, the datagram reader, and the resource-stream
/// reader. Dropping the transport drops the event senders; the browser closes
/// the connection when the JS object is collected.
pub struct BrowserTransport {
    transport: WebTransport,
    map: ChannelMap,
    max_payload: usize,
    datagram_limit: usize,
    writer: mpsc::UnboundedSender<Vec<u8>>,
    events: mpsc::UnboundedReceiver<TransportEvent>,
    resources: mpsc::Receiver<Result<Bytes, WebTransportError>>,
    datagrams: WritableStreamDefaultWriter,
    closed: Cell<Option<CloseReason>>,
    close_sent: Cell<bool>,
}

impl std::fmt::Debug for BrowserTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrowserTransport")
            .field("channels", &self.map.len())
            .field("closed", &self.closed.get())
            .finish()
    }
}

impl BrowserTransport {
    /// Open the transport and start its background tasks.
    ///
    /// This awaits the browser's `ready` promise, so it can reject for
    /// certificate, protocol, or network reasons. There is no internal
    /// timeout: wrap the future in the caller's `AbortSignal` or timer.
    pub async fn connect(options: BrowserConnectOptions) -> Result<Self, WebTransportError> {
        if !options.url.starts_with("https://") {
            return Err(WebTransportError::Unsupported(format!(
                "WebTransport requires an https:// URL, got '{}'",
                options.url
            )));
        }
        if options.channel_map.is_empty() {
            return Err(WebTransportError::Protocol(
                "the negotiated channel map is empty".into(),
            ));
        }

        let hash = WebTransportHash::new();
        hash.set_algorithm("sha-256");
        hash.set_value_u8_array(&js_sys::Uint8Array::from(
            options.certificate_sha256.as_slice(),
        ));

        let js_options = WebTransportOptions::new();
        js_options.set_server_certificate_hashes(&[hash]);
        js_options.set_require_unreliable(true);
        js_options.set_allow_pooling(false);

        let transport = WebTransport::new_with_options(&options.url, &js_options)
            .map_err(|error| classify_connect_error(&error))?;

        JsFuture::from(transport.ready())
            .await
            .map_err(|error| classify_connect_error(&error))?;

        let datagrams: WebTransportDatagramDuplexStream = transport.datagrams();
        let datagram_limit = datagrams.max_datagram_size() as usize;
        if datagram_limit <= 1 {
            let _ = transport.close();
            return Err(WebTransportError::Unsupported(
                "the connection did not negotiate WebTransport datagrams".into(),
            ));
        }
        let datagrams_writer = datagrams
            .writable()
            .get_writer()
            .map_err(|error| WebTransportError::Io(js_message(&error)))?;

        let bidi = JsFuture::from(transport.create_bidirectional_stream())
            .await
            .map_err(|error| classify_connect_error(&error))?;
        let bidi: WebTransportBidirectionalStream = bidi.unchecked_into();
        let reader: ReadableStreamDefaultReader = bidi
            .readable()
            .unchecked_into::<ReadableStream>()
            .get_reader()
            .unchecked_into();
        let writer = bidi
            .writable()
            .unchecked_into::<WritableStream>()
            .get_writer()
            .map_err(|error| WebTransportError::Io(js_message(&error)))?;

        let (event_tx, events) = mpsc::unbounded();
        let (write_tx, write_rx) = mpsc::unbounded();
        let (resource_tx, resources) = mpsc::channel(4);
        let max_payload = options.max_payload;

        spawn_writer_task(writer, write_rx, event_tx.clone());
        spawn_stream_task(reader, max_payload, event_tx.clone());
        spawn_datagram_task(datagrams, event_tx.clone());
        spawn_resource_task(transport.clone(), resource_tx);
        spawn_closed_task(transport.clone(), event_tx);

        Ok(Self {
            transport,
            map: options.channel_map,
            max_payload,
            datagram_limit: datagram_limit.min(max_payload.max(1)),
            writer: write_tx,
            events,
            resources,
            datagrams: datagrams_writer,
            closed: Cell::new(None),
            close_sent: Cell::new(false),
        })
    }

    /// The negotiated channel map.
    pub fn channel_map(&self) -> &ChannelMap {
        &self.map
    }

    /// The browser's maximum datagram payload size.
    pub fn max_datagram_size(&self) -> usize {
        self.datagram_limit
    }

    /// The first channel of `kind`, if negotiated.
    pub fn channel_for_kind(&self, kind: ChannelKind) -> Option<ChannelId> {
        self.map
            .iter()
            .find(|descriptor| descriptor.kind == kind)
            .map(|descriptor| ChannelId::new(descriptor.id))
    }

    /// Send one bounded resource on its own unidirectional stream.
    ///
    /// This is separate from [`ChannelTransport`] because native transports
    /// may not support it. The node uses it for encoded scene images.
    pub async fn send_resource(&self, payload: Bytes) -> Result<(), WebTransportError> {
        if let Some(reason) = self.closed.get() {
            return Err(WebTransportError::Closed(reason.as_str().into()));
        }
        let stream = JsFuture::from(self.transport.create_unidirectional_stream())
            .await
            .map_err(|error| classify_io_error(&error))?;
        let stream: WritableStream = stream.unchecked_into();
        let writer = stream
            .get_writer()
            .map_err(|error| WebTransportError::Io(js_message(&error)))?;
        let chunk = js_sys::Uint8Array::from(payload.as_ref());
        JsFuture::from(writer.write_with_chunk(chunk.as_ref()))
            .await
            .map_err(|error| classify_io_error(&error))?;
        JsFuture::from(writer.close())
            .await
            .map_err(|error| classify_io_error(&error))?;
        Ok(())
    }

    /// Wait for the next resource the node sends on a unidirectional stream.
    ///
    /// Resources are queued in arrival order. The queue is bounded, so the
    /// reader task applies backpressure if the caller stops consuming.
    pub async fn next_resource(&mut self) -> Result<Bytes, WebTransportError> {
        match self.resources.next().await {
            Some(Ok(bytes)) => Ok(bytes),
            Some(Err(error)) => Err(error),
            None => Err(WebTransportError::Closed(
                "the resource stream reader stopped".into(),
            )),
        }
    }

    /// Close with an explicit reason. Idempotent.
    pub fn close(&self, reason: CloseReason) {
        if self.close_sent.replace(true) {
            return;
        }
        self.closed.set(Some(reason));
        let info = WebTransportCloseInfo::new();
        info.set_close_code(close_code(reason));
        info.set_reason(reason.as_str());
        self.transport.close_with_close_info(&info);
    }
}

#[async_trait(?Send)]
impl ChannelTransport for BrowserTransport {
    fn mode(&self) -> TransportMode {
        TransportMode::Quic
    }

    fn channel_map(&self) -> &ChannelMap {
        &self.map
    }

    async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<(), ApiError> {
        if let Some(reason) = self.closed.get() {
            return Err(WebTransportError::Closed(reason.as_str().into()).into_api_error());
        }
        let descriptor = self.map.require(channel)?;
        if payload.len() > self.max_payload {
            return Err(ozma_transport::payload_too_large(
                channel,
                payload.len(),
                self.max_payload,
            ));
        }
        if payload.len() > descriptor.max_payload as usize {
            return Err(ozma_transport::payload_too_large(
                channel,
                payload.len(),
                descriptor.max_payload as usize,
            ));
        }

        if descriptor.reliable {
            let frame = encode_reliable(channel, &payload)?;
            self.writer.unbounded_send(frame).map_err(|_| {
                WebTransportError::Closed("the reliable writer task stopped".into())
                    .into_api_error()
            })?;
            return Ok(());
        }

        let datagram = encode_datagram(channel, &payload, Some(self.datagram_limit))?;
        let chunk = js_sys::Uint8Array::from(datagram.as_slice());
        JsFuture::from(self.datagrams.write_with_chunk(chunk.as_ref()))
            .await
            .map_err(|error| classify_io_error(&error).into_api_error())?;
        Ok(())
    }

    async fn recv(&mut self) -> TransportEvent {
        if let Some(reason) = self.closed.get() {
            return TransportEvent::Closed { reason };
        }
        match self.events.next().await {
            Some(event) => {
                if let TransportEvent::Closed { reason } = event {
                    self.closed.set(Some(reason));
                }
                event
            }
            None => {
                let reason = CloseReason::PeerClosed;
                self.closed.set(Some(reason));
                TransportEvent::Closed { reason }
            }
        }
    }

    async fn close_with_reason(&self, reason: CloseReason) -> Result<(), ApiError> {
        self.close(reason);
        Ok(())
    }
}

fn spawn_writer_task(
    writer: WritableStreamDefaultWriter,
    mut frames: mpsc::UnboundedReceiver<Vec<u8>>,
    events: mpsc::UnboundedSender<TransportEvent>,
) {
    spawn_local(async move {
        while let Some(frame) = frames.next().await {
            let chunk = js_sys::Uint8Array::from(frame.as_slice());
            if let Err(error) = JsFuture::from(writer.write_with_chunk(chunk.as_ref())).await {
                let _ = events.unbounded_send(TransportEvent::Error {
                    error: classify_io_error(&error).into_api_error(),
                });
                let _ = events.unbounded_send(TransportEvent::Closed {
                    reason: CloseReason::TransportError,
                });
                return;
            }
        }
        // All senders were dropped: half-close the reliable stream.
        let _ = JsFuture::from(writer.close()).await;
    });
}

fn spawn_stream_task(
    reader: ReadableStreamDefaultReader,
    max_payload: usize,
    events: mpsc::UnboundedSender<TransportEvent>,
) {
    spawn_local(async move {
        let mut decoder = FrameDecoder::new(max_payload);
        loop {
            match read_chunk(&reader).await {
                Ok(Some(value)) => {
                    let bytes = js_sys::Uint8Array::new(&value).to_vec();
                    match decoder.push(&bytes) {
                        Ok(frames) => {
                            for (channel, payload) in frames {
                                if events
                                    .unbounded_send(TransportEvent::Payload { channel, payload })
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                        Err(error) => {
                            let _ = events.unbounded_send(TransportEvent::Error { error });
                            let _ = events.unbounded_send(TransportEvent::Closed {
                                reason: CloseReason::ProtocolError,
                            });
                            return;
                        }
                    }
                }
                Ok(None) => {
                    let _ = events.unbounded_send(TransportEvent::Closed {
                        reason: CloseReason::PeerClosed,
                    });
                    return;
                }
                Err(error) => {
                    let _ = events.unbounded_send(TransportEvent::Error {
                        error: error.into_api_error(),
                    });
                    let _ = events.unbounded_send(TransportEvent::Closed {
                        reason: CloseReason::TransportError,
                    });
                    return;
                }
            }
        }
    });
}

fn spawn_datagram_task(
    datagrams: WebTransportDatagramDuplexStream,
    events: mpsc::UnboundedSender<TransportEvent>,
) {
    spawn_local(async move {
        let reader: ReadableStreamDefaultReader =
            datagrams.readable().get_reader().unchecked_into();
        loop {
            match read_chunk(&reader).await {
                Ok(Some(value)) => {
                    let bytes = js_sys::Uint8Array::new(&value).to_vec();
                    match FrameDecoder::decode_datagram(&bytes) {
                        Ok((channel, payload)) => {
                            if events
                                .unbounded_send(TransportEvent::Datagram { channel, payload })
                                .is_err()
                            {
                                return;
                            }
                        }
                        Err(error) => {
                            // A malformed datagram is not fatal: drop it and
                            // keep the media channel alive.
                            let _ = events.unbounded_send(TransportEvent::Error { error });
                        }
                    }
                }
                Ok(None) => {
                    let _ = events.unbounded_send(TransportEvent::Closed {
                        reason: CloseReason::PeerClosed,
                    });
                    return;
                }
                Err(error) => {
                    let _ = events.unbounded_send(TransportEvent::Error {
                        error: error.into_api_error(),
                    });
                    let _ = events.unbounded_send(TransportEvent::Closed {
                        reason: CloseReason::TransportError,
                    });
                    return;
                }
            }
        }
    });
}

fn spawn_resource_task(
    transport: WebTransport,
    mut resources: mpsc::Sender<Result<Bytes, WebTransportError>>,
) {
    spawn_local(async move {
        let reader: ReadableStreamDefaultReader = transport
            .incoming_unidirectional_streams()
            .get_reader()
            .unchecked_into();
        loop {
            match read_chunk(&reader).await {
                Ok(Some(value)) => {
                    let stream: ReadableStream = value.unchecked_into();
                    let result = read_all(&stream).await;
                    if resources.send(result).await.is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(error) => {
                    let _ = resources.send(Err(error)).await;
                    return;
                }
            }
        }
    });
}

fn spawn_closed_task(transport: WebTransport, events: mpsc::UnboundedSender<TransportEvent>) {
    spawn_local(async move {
        let _ = JsFuture::from(transport.closed()).await;
        let _ = events.unbounded_send(TransportEvent::Closed {
            reason: CloseReason::PeerClosed,
        });
    });
}

/// Read one chunk from a stream reader. `Ok(None)` means end of stream.
async fn read_chunk(
    reader: &ReadableStreamDefaultReader,
) -> Result<Option<JsValue>, WebTransportError> {
    let result = JsFuture::from(reader.read())
        .await
        .map_err(|error| classify_io_error(&error))?;
    let done = js_sys::Reflect::get(&result, &JsValue::from_str("done"))
        .ok()
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if done {
        return Ok(None);
    }
    let value = js_sys::Reflect::get(&result, &JsValue::from_str("value"))
        .map_err(|error| WebTransportError::Io(js_message(&error)))?;
    if value.is_null() || value.is_undefined() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

/// Drain a resource stream into one byte vector.
async fn read_all(stream: &ReadableStream) -> Result<Bytes, WebTransportError> {
    let reader: ReadableStreamDefaultReader = stream.get_reader().unchecked_into();
    let mut buffer = Vec::new();
    while let Some(value) = read_chunk(&reader).await? {
        buffer.extend_from_slice(&js_sys::Uint8Array::new(&value).to_vec());
    }
    Ok(Bytes::from(buffer))
}

fn close_code(reason: CloseReason) -> u32 {
    match reason {
        CloseReason::Normal => 0,
        CloseReason::LocalShutdown => 1,
        CloseReason::PeerClosed => 2,
        CloseReason::Timeout => 3,
        CloseReason::TransportError => 4,
        CloseReason::ProtocolError => 5,
    }
}

fn classify_connect_error(value: &JsValue) -> WebTransportError {
    let message = js_message(value);
    let lower = message.to_ascii_lowercase();
    if lower.contains("certificate") || lower.contains("fingerprint") || lower.contains("hash") {
        WebTransportError::Certificate(message)
    } else if lower.contains("not supported")
        || lower.contains("unsupported")
        || (lower.contains("webtransport") && lower.contains("disabled"))
    {
        WebTransportError::Unsupported(message)
    } else {
        WebTransportError::Connect(message)
    }
}

fn classify_io_error(value: &JsValue) -> WebTransportError {
    let message = js_message(value);
    let lower = message.to_ascii_lowercase();
    if lower.contains("closed") || lower.contains("close") {
        WebTransportError::Closed(message)
    } else {
        WebTransportError::Io(message)
    }
}

/// Render a JS exception as a readable message instead of `JsValue` debug.
fn js_message(value: &JsValue) -> String {
    if let Some(error) = value.dyn_ref::<js_sys::Error>() {
        let name = String::from(error.name());
        let message = String::from(error.message());
        return if name.is_empty() {
            message
        } else {
            format!("{name}: {message}")
        };
    }
    if let Some(text) = value.as_string() {
        return text;
    }
    format!("{value:?}")
}

/// SHA-256 of a DER certificate, ready for `serverCertificateHashes`.
pub fn certificate_sha256_der(der: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(der);
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&digest);
    hash
}

/// SHA-256 of base64 DER, as served by `/api/v1/transport`.
pub fn certificate_sha256_from_base64(
    certificate_der_base64: &str,
) -> Result<[u8; 32], WebTransportError> {
    let der = STANDARD
        .decode(certificate_der_base64.trim())
        .or_else(|_| URL_SAFE_NO_PAD.decode(certificate_der_base64.trim()))
        .map_err(|error| {
            WebTransportError::Certificate(format!(
                "the certificate_der_base64 value is not valid base64: {error}"
            ))
        })?;
    if der.is_empty() {
        return Err(WebTransportError::Certificate(
            "the certificate_der_base64 value is empty".into(),
        ));
    }
    Ok(certificate_sha256_der(&der))
}

/// Parse a hex SHA-256 fingerprint (`aa:bb:...`, `sha-256:...`, or plain hex).
pub fn certificate_sha256_from_fingerprint(
    fingerprint: &str,
) -> Result<[u8; 32], WebTransportError> {
    let trimmed = fingerprint.trim();
    let without_prefix = trimmed
        .strip_prefix("sha-256:")
        .or_else(|| trimmed.strip_prefix("sha256:"))
        .unwrap_or(trimmed);
    let digits: String = without_prefix
        .chars()
        .filter(|character| *character != ':' && *character != ' ' && *character != '-')
        .collect();
    if digits.len() != 64 {
        return Err(WebTransportError::Certificate(format!(
            "a SHA-256 fingerprint must be 64 hex digits, got {}",
            digits.len()
        )));
    }
    let mut hash = [0u8; 32];
    for (index, slot) in hash.iter_mut().enumerate() {
        let pair = &digits[index * 2..index * 2 + 2];
        *slot = u8::from_str_radix(pair, 16).map_err(|_| {
            WebTransportError::Certificate(format!(
                "the certificate fingerprint contains non-hex characters: {pair}"
            ))
        })?;
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_and_hex_helpers_agree() {
        let der = b"not a real certificate";
        let hash = certificate_sha256_der(der);
        let encoded = STANDARD.encode(der);
        assert_eq!(certificate_sha256_from_base64(&encoded).unwrap(), hash);
        let fingerprint = hash
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            certificate_sha256_from_fingerprint(&fingerprint).unwrap(),
            hash
        );
        let colonized = hash
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(
            certificate_sha256_from_fingerprint(&colonized).unwrap(),
            hash
        );
    }

    #[test]
    fn bad_fingerprints_are_typed() {
        assert!(matches!(
            certificate_sha256_from_fingerprint("abcd").unwrap_err(),
            WebTransportError::Certificate(_)
        ));
        assert!(matches!(
            certificate_sha256_from_base64("!!!!").unwrap_err(),
            WebTransportError::Certificate(_)
        ));
    }

    #[test]
    fn payload_too_large_maps_to_a_channel_error() {
        let error = WebTransportError::PayloadTooLarge {
            channel: 2,
            len: 2000,
            max: 1200,
        }
        .into_api_error();
        assert_eq!(error.code, ozma_protocol::codes::INVALID_REQUEST);
    }

    #[test]
    fn descriptor_matching_requires_https_quic() {
        use ozma_protocol::TransportMode;
        let mut descriptor = TransportDescriptor {
            mode: TransportMode::Quic,
            endpoint: Some("https://node.example:4433".into()),
            secure: true,
            alpn: Some("ozma/1".into()),
            certificate_fingerprint: Some("00".repeat(32)),
            channels: Vec::new(),
        };
        assert!(BrowserConnectOptions::matches(&descriptor));
        let options = BrowserConnectOptions::from_descriptor(&descriptor).unwrap();
        assert_eq!(options.url, "https://node.example:4433");
        assert_eq!(options.channel_map.len(), 7);

        descriptor.endpoint = Some("192.0.2.1:4433".into());
        assert!(!BrowserConnectOptions::matches(&descriptor));
    }
}
