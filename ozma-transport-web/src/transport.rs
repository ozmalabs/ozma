// SPDX-License-Identifier: AGPL-3.0-only
//! The wasm32 WebTransport adapter.
//!
//! [`BrowserTransport`] owns the receive half of a WebTransport session, while
//! [`BrowserSender`] is a cloneable handle to its send half. Both share one
//! [`crate::SendQueue`], so reliable frames from concurrent senders stay whole
//! and ordered, and one `Rc<Shared>` closed flag, so a close is visible to
//! every clone immediately.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use ozma_client_core::framing::{encode_datagram, FrameDecoder};
use ozma_client_core::ChannelTransport;
use ozma_protocol::{ApiError, ChannelKind, TransportMode};
use ozma_transport::{ChannelId, ChannelMap, CloseReason, TransportEvent};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{
    ReadableStream, ReadableStreamDefaultReader, WebTransport, WebTransportBidirectionalStream,
    WebTransportCloseInfo, WebTransportDatagramDuplexStream, WebTransportHash, WebTransportOptions,
    WritableStream, WritableStreamDefaultWriter,
};

use crate::queue::{FrameWriter, SendQueue, SendQueueError};
use crate::{BrowserConnectOptions, WebTransportError};

impl FrameWriter for mpsc::UnboundedSender<Vec<u8>> {
    type Error = ();

    fn write_frame(&self, frame: Vec<u8>) -> Result<(), Self::Error> {
        self.unbounded_send(frame).map_err(|_| ())
    }
}

/// State shared by [`BrowserTransport`] and every [`BrowserSender`] clone.
///
/// wasm is single-threaded, so `Rc`/`Cell`/`RefCell` are enough. The reliable
/// writer is a synchronous `SendQueue`, so no borrow is held across an
/// `.await`; the datagram writer is cloned out of its `RefCell` before a write
/// future is awaited.
struct Shared {
    transport: WebTransport,
    map: ChannelMap,
    max_payload: usize,
    datagram_limit: usize,
    writer: SendQueue<mpsc::UnboundedSender<Vec<u8>>>,
    datagrams: RefCell<WritableStreamDefaultWriter>,
    events: mpsc::UnboundedSender<TransportEvent>,
    closed: Cell<Option<CloseReason>>,
    close_sent: Cell<bool>,
}

impl Shared {
    /// Validate one outbound payload and report whether its channel is
    /// reliable.
    fn validate_send(&self, channel: ChannelId, len: usize) -> Result<bool, WebTransportError> {
        if let Some(reason) = self.closed.get() {
            return Err(WebTransportError::Closed(reason.as_str().into()));
        }
        let descriptor = self
            .map
            .require(channel)
            .map_err(|error| WebTransportError::Protocol(error.message))?;
        if len > self.max_payload {
            return Err(WebTransportError::PayloadTooLarge {
                channel: channel.raw(),
                len,
                max: self.max_payload,
            });
        }
        if len > descriptor.max_payload as usize {
            return Err(WebTransportError::PayloadTooLarge {
                channel: channel.raw(),
                len,
                max: descriptor.max_payload as usize,
            });
        }
        Ok(descriptor.reliable)
    }

    /// Enqueue one whole reliable frame. The queue writes synchronously, so
    /// concurrent clones cannot interleave two frames.
    fn send_reliable(&self, channel: ChannelId, payload: &[u8]) -> Result<(), WebTransportError> {
        self.writer
            .send(channel, payload)
            .map_err(|error| match error {
                SendQueueError::Encode(error) => WebTransportError::Protocol(error.message),
                SendQueueError::Writer(()) => {
                    WebTransportError::Closed("the reliable writer task stopped".into())
                }
            })
    }

    /// Close the transport once and wake any pending `recv`.
    fn close(&self, reason: CloseReason) {
        if self.close_sent.replace(true) {
            return;
        }
        let already_closed = self.closed.replace(Some(reason)).is_some();
        // Resolve a pending recv immediately instead of waiting for the
        // browser's `closed` promise. The receive task may also report a close
        // later; recv is sticky once the flag is set.
        let _ = self
            .events
            .unbounded_send(TransportEvent::Closed { reason });
        if already_closed {
            // A peer close was already observed, so the JS transport is gone;
            // calling close again would raise InvalidStateError.
            return;
        }
        let info = WebTransportCloseInfo::new();
        info.set_close_code(close_code(reason));
        info.set_reason(reason.as_str());
        self.transport.close_with_close_info(&info);
    }
}

/// A cloneable send handle for a [`BrowserTransport`].
///
/// Reliable channels (control, input, telemetry, bulk) go through the shared
/// serialized writer. Unreliable channels (video, camera, microphone) go out as
/// datagrams. Neither path waits for [`ChannelTransport::recv`], so a facade can
/// send input or uplink media while the media loop is between events.
#[derive(Clone)]
pub struct BrowserSender {
    shared: Rc<Shared>,
}

impl BrowserSender {
    /// Send on one channel.
    ///
    /// Reliable channels are encoded and queued as (or one) whole frame(s) on
    /// the shared writer; unreliable channels are written as one datagram.
    pub async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<(), WebTransportError> {
        let reliable = self.shared.validate_send(channel, payload.len())?;

        if reliable {
            self.shared.send_reliable(channel, &payload)?;
            return Ok(());
        }

        if payload.len() + 1 > self.shared.datagram_limit {
            return Err(WebTransportError::PayloadTooLarge {
                channel: channel.raw(),
                len: payload.len(),
                max: self.shared.datagram_limit.saturating_sub(1),
            });
        }
        let datagram = encode_datagram(channel, &payload, None)
            .map_err(|error| WebTransportError::Protocol(error.message))?;
        // Clone the JS handle out of the cell so no borrow is held across the
        // write promise. Concurrent clones still target the same writer, and
        // the WritableStream serializes its own writes one by one.
        let writer = self.shared.datagrams.borrow().clone();
        let chunk = js_sys::Uint8Array::from(datagram.as_slice());
        JsFuture::from(writer.write_with_chunk(chunk.as_ref()))
            .await
            .map_err(|error| classify_io_error(&error))?;
        Ok(())
    }

    /// The first channel of `kind` in the negotiated map.
    pub fn channel_for_kind(&self, kind: ChannelKind) -> Option<ChannelId> {
        self.shared
            .map
            .iter()
            .find(|descriptor| descriptor.kind == kind)
            .map(|descriptor| ChannelId::new(descriptor.id))
    }

    /// The browser's maximum datagram payload size, including the channel byte.
    ///
    /// Callers that fragment uplink media should keep `payload.len() + 1` at or
    /// below this value.
    pub fn max_datagram_size(&self) -> usize {
        self.shared.datagram_limit
    }

    /// True once a close was requested or observed.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.get().is_some()
    }

    /// Close the transport with `reason`. Idempotent.
    ///
    /// This does not send `ReleaseAll` or `Bye`; the caller must send those on
    /// the control channel first. A pending [`ChannelTransport::recv`] resolves
    /// to `Closed` immediately.
    pub fn close(&self, reason: CloseReason) {
        self.shared.close(reason);
    }
}

/// A connected browser WebTransport session.
///
/// The transport owns four background tasks: the reliable write serializer,
/// the reliable stream reader, the datagram reader, and the resource-stream
/// reader. Dropping the transport drops the event senders; the browser closes
/// the connection when the JS object is collected.
pub struct BrowserTransport {
    shared: Rc<Shared>,
    events: mpsc::UnboundedReceiver<TransportEvent>,
    resources: mpsc::Receiver<Result<Bytes, WebTransportError>>,
}

impl std::fmt::Debug for BrowserTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrowserTransport")
            .field("channels", &self.shared.map.len())
            .field("closed", &self.shared.closed.get())
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
        spawn_closed_task(transport.clone(), event_tx.clone());

        let shared = Rc::new(Shared {
            transport,
            map: options.channel_map,
            max_payload,
            datagram_limit: datagram_limit.min(max_payload.max(1)),
            writer: SendQueue::new(write_tx),
            datagrams: RefCell::new(datagrams_writer),
            events: event_tx,
            closed: Cell::new(None),
            close_sent: Cell::new(false),
        });

        Ok(Self {
            shared,
            events,
            resources,
        })
    }

    /// A cloneable handle to the send half.
    ///
    /// Keep one outside the media loop to send input or uplink media while a
    /// receive is pending.
    pub fn sender(&self) -> BrowserSender {
        BrowserSender {
            shared: Rc::clone(&self.shared),
        }
    }

    /// The negotiated channel map.
    pub fn channel_map(&self) -> &ChannelMap {
        &self.shared.map
    }

    /// The browser's maximum datagram payload size.
    pub fn max_datagram_size(&self) -> usize {
        self.shared.datagram_limit
    }

    /// The first channel of `kind`, if negotiated.
    pub fn channel_for_kind(&self, kind: ChannelKind) -> Option<ChannelId> {
        self.sender().channel_for_kind(kind)
    }

    /// Send one bounded resource on its own unidirectional stream.
    ///
    /// This is separate from [`ChannelTransport`] because native transports
    /// may not support it. The node uses it for encoded scene images.
    pub async fn send_resource(&self, payload: Bytes) -> Result<(), WebTransportError> {
        if let Some(reason) = self.shared.closed.get() {
            return Err(WebTransportError::Closed(reason.as_str().into()));
        }
        let stream = JsFuture::from(self.shared.transport.create_unidirectional_stream())
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
        self.shared.close(reason);
    }
}

#[async_trait(?Send)]
impl ChannelTransport for BrowserTransport {
    fn mode(&self) -> TransportMode {
        TransportMode::Quic
    }

    fn channel_map(&self) -> &ChannelMap {
        &self.shared.map
    }

    async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<(), ApiError> {
        self.sender()
            .send(channel, payload)
            .await
            .map_err(WebTransportError::into_api_error)
    }

    async fn recv(&mut self) -> TransportEvent {
        if let Some(reason) = self.shared.closed.get() {
            return TransportEvent::Closed { reason };
        }
        match self.events.next().await {
            Some(event) => {
                if let TransportEvent::Closed { reason } = event {
                    self.shared.closed.set(Some(reason));
                }
                event
            }
            None => {
                let reason = CloseReason::PeerClosed;
                self.shared.closed.set(Some(reason));
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
