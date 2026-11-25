//! Bounded, injectable packet pump between a mesh TUN link and WireGuard.
//!
//! The dataplane creates and routes a persistent TUN interface, but a link with
//! no reader/writer is useless. [`PacketPump`] owns that data path:
//!
//! ```text
//! TUN reader ──bounded queue──▶ tunnel sender ──▶ boringtun ──▶ UDP
//! TUN writer ◀─bounded queue── tunnel receiver ◀── boringtun ◀── UDP
//! ```
//!
//! Two traits keep the default tests free of root, `/dev/net/tun`, and real
//! WireGuard sockets:
//!
//! * [`TunIo`] — one IPv4 packet in, one packet out.
//! * [`TunnelIo`] — encrypt/send an IPv4 packet, receive a decrypted packet.
//!
//! Production values are [`SystemTunProvider`] (a safe `tun-rs` wrapper) and
//! [`ManagerPacketIo`] (the packet API added to [`crate::MeshManager`]). Tests
//! inject in-memory channels and scripted partial reads/writes.
//!
//! Every direction has a bounded queue. A full queue increments a drop counter
//! and drops the packet instead of growing memory or blocking the other
//! direction. Per-direction packet/byte counters and error counters are exposed
//! in [`PumpStats`]. A worker error stops both directions and leaves the pump in
//! [`PumpState::Failed`] with a typed `mesh.pump-*` error; it never reports a
//! fake running state or fabricated counters.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::manager::MeshManager;

/// Default number of packets buffered per direction.
pub const DEFAULT_QUEUE_CAPACITY: usize = 256;
/// Largest accepted per-direction queue bound.
pub const MAX_QUEUE_CAPACITY: usize = 4_096;
/// Smallest usable IPv4 packet buffer (minimum IPv4 MTU).
pub const MIN_PACKET_BUFFER: usize = 68;
/// Largest accepted packet buffer (the TUN/WireGuard maximum).
pub const MAX_PACKET_BUFFER: usize = 65_535;

/// Typed packet-pump failure.
///
/// `code` is stable and controller-friendly. `reason` never contains key
/// material. Codes use the `mesh.tun-*` / `mesh.pump-*` namespace so the node
/// can distinguish a TUN attach failure from a tunnel or queue failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PumpError {
    pub code: String,
    pub reason: String,
}

impl PumpError {
    pub fn new(code: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            reason: reason.into(),
        }
    }

    /// The persistent TUN interface could not be opened/attached.
    pub fn tun_open(reason: impl Into<String>) -> Self {
        Self::new("mesh.tun-open", reason)
    }

    /// Reading one packet from the TUN fd failed.
    pub fn tun_read(reason: impl Into<String>) -> Self {
        Self::new("mesh.pump-tun-read", reason)
    }

    /// Writing one packet to the TUN fd failed.
    pub fn tun_write(reason: impl Into<String>) -> Self {
        Self::new("mesh.pump-tun-write", reason)
    }

    /// Encapsulating or sending a packet through the tunnel failed.
    pub fn tunnel_send(reason: impl Into<String>) -> Self {
        Self::new("mesh.pump-tunnel-send", reason)
    }

    /// Receiving or decapsulating a packet from the tunnel failed.
    pub fn tunnel_receive(reason: impl Into<String>) -> Self {
        Self::new("mesh.pump-tunnel-receive", reason)
    }

    /// The tunnel has no packet API or is not running.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::new("mesh.pump-unavailable", reason)
    }

    /// Queue/buffer configuration is outside the supported bounds.
    pub fn invalid(reason: impl Into<String>) -> Self {
        Self::new("mesh.pump-invalid", reason)
    }

    /// A packet larger than the configured bounded buffer was produced.
    pub fn packet_too_large(size: usize, limit: usize) -> Self {
        Self::new(
            "mesh.pump-packet-too-large",
            format!("packet of {size} bytes exceeds the {limit}-byte pump buffer"),
        )
    }
}

impl std::fmt::Display for PumpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.reason)
    }
}

impl std::error::Error for PumpError {}

/// Packet-pump lifecycle. Distinct from tunnel handshake and dataplane state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PumpState {
    /// No pump was requested.
    NotConfigured,
    /// Configured but no workers are running.
    Stopped,
    /// Workers are being created.
    Starting,
    /// Both directions are running.
    Running,
    /// A worker failed; both directions are stopped.
    Failed,
}

/// Bounded configuration for one pump.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PumpConfig {
    /// Maximum queued packets in each direction.
    pub queue_capacity: usize,
    /// Fixed read buffer for one IPv4 packet.
    pub packet_buffer: usize,
}

impl Default for PumpConfig {
    fn default() -> Self {
        Self {
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            packet_buffer: MAX_PACKET_BUFFER,
        }
    }
}

impl PumpConfig {
    fn validate(self) -> Result<Self, PumpError> {
        if !(1..=MAX_QUEUE_CAPACITY).contains(&self.queue_capacity) {
            return Err(PumpError::invalid(format!(
                "queue capacity {} must be in 1..={MAX_QUEUE_CAPACITY}",
                self.queue_capacity
            )));
        }
        if !(MIN_PACKET_BUFFER..=MAX_PACKET_BUFFER).contains(&self.packet_buffer) {
            return Err(PumpError::invalid(format!(
                "packet buffer {} must be in {MIN_PACKET_BUFFER}..={MAX_PACKET_BUFFER}",
                self.packet_buffer
            )));
        }
        Ok(self)
    }
}

/// Honest pump counters. All values start at zero and only real packets or
/// errors increment them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PumpStats {
    /// Packets read from TUN.
    pub tun_read_packets: u64,
    /// Bytes read from TUN.
    pub tun_read_bytes: u64,
    /// Packets successfully handed to the tunnel encrypt/send path.
    pub tun_to_tunnel_packets: u64,
    /// Bytes successfully handed to the tunnel encrypt/send path.
    pub tun_to_tunnel_bytes: u64,
    /// Packets received/decapsulated from the tunnel.
    pub tunnel_received_packets: u64,
    /// Bytes received/decapsulated from the tunnel.
    pub tunnel_received_bytes: u64,
    /// Packets fully written to TUN.
    pub tunnel_to_tun_packets: u64,
    /// Bytes fully written to TUN.
    pub tunnel_to_tun_bytes: u64,
    /// TUN→tunnel packets dropped because the bounded queue was full.
    pub dropped_tun_to_tunnel: u64,
    /// Tunnel→TUN packets dropped because the bounded queue was full.
    pub dropped_tunnel_to_tun: u64,
    /// TUN read failures.
    pub tun_read_errors: u64,
    /// TUN write failures.
    pub tun_write_errors: u64,
    /// Tunnel send/encapsulation failures.
    pub tunnel_send_errors: u64,
    /// Tunnel receive/decapsulation failures.
    pub tunnel_receive_errors: u64,
}

/// Snapshot of pump state for `/api/v1/connect/status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PumpStatus {
    /// Whether a pump was requested by the configuration.
    pub configured: bool,
    pub state: PumpState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    /// Bound used by both per-direction queues.
    pub queue_capacity: usize,
    pub stats: PumpStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<PumpError>,
}

impl PumpStatus {
    /// Status for a configured pump that has not started (no fake counters).
    pub fn stopped(interface: Option<String>, queue_capacity: usize) -> Self {
        Self {
            configured: true,
            state: PumpState::Stopped,
            interface,
            queue_capacity,
            stats: PumpStats::default(),
            last_error: None,
        }
    }
}

/// Injectable TUN data path.
///
/// `read_packet` returns one complete IPv4 packet (a short read is a complete
/// short packet, as with the kernel TUN driver). `write_packet` may write fewer
/// bytes than supplied; the pump retries the remainder.
#[async_trait]
pub trait TunIo: Send + Sync + 'static {
    async fn read_packet(&self, buffer: &mut [u8]) -> Result<usize, PumpError>;
    async fn write_packet(&self, packet: &[u8]) -> Result<usize, PumpError>;
}

/// Injectable tunnel data path.
///
/// `send_packet` receives a complete IPv4 packet and must route it to the peer
/// that owns the destination address. `recv_packet` returns one decrypted IPv4
/// packet, copying it into `buffer`. Both methods are cancellation-safe: the
/// pump drops the future when it is shut down.
#[async_trait]
pub trait TunnelIo: Send + Sync + 'static {
    async fn send_packet(&self, packet: &[u8]) -> Result<(), PumpError>;
    async fn recv_packet(&self, buffer: &mut [u8]) -> Result<usize, PumpError>;
}

/// Opens the persistent TUN interface created by the dataplane.
#[async_trait]
pub trait TunProvider: Send + Sync + 'static {
    async fn open(&self, interface: &str) -> Result<Arc<dyn TunIo>, PumpError>;
}

/// Production TUN provider: attaches to an existing persistent Linux TUN link.
///
/// The `tun-rs` crate owns the `TUNSETIFF` ioctl and the readiness integration;
/// this crate contains no `unsafe`. The existing link's address, MTU, routes,
/// and up/down state are left untouched (`packet_information(false)` and
/// `inherit_enable_state`).
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemTunProvider;

impl SystemTunProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl TunProvider for SystemTunProvider {
    async fn open(&self, interface: &str) -> Result<Arc<dyn TunIo>, PumpError> {
        let name = interface.to_owned();
        let device = tokio::task::spawn_blocking(move || {
            tun_rs::DeviceBuilder::new()
                .name(name)
                .packet_information(false)
                .inherit_enable_state()
                .build_async()
        })
        .await
        .map_err(|error| PumpError::tun_open(format!("TUN open task failed: {error}")))?
        .map_err(|error| {
            PumpError::tun_open(format!(
                "cannot attach TUN interface {interface:?} via /dev/net/tun: {error}"
            ))
        })?;
        Ok(Arc::new(SystemTunIo { device }))
    }
}

struct SystemTunIo {
    device: tun_rs::AsyncDevice,
}

#[async_trait]
impl TunIo for SystemTunIo {
    async fn read_packet(&self, buffer: &mut [u8]) -> Result<usize, PumpError> {
        self.device
            .recv(buffer)
            .await
            .map_err(|error| PumpError::tun_read(format!("TUN read failed: {error}")))
    }

    async fn write_packet(&self, packet: &[u8]) -> Result<usize, PumpError> {
        self.device
            .send(packet)
            .await
            .map_err(|error| PumpError::tun_write(format!("TUN write failed: {error}")))
    }
}

/// Production tunnel adapter over the packet API added to
/// [`crate::MeshManager`].
///
/// The manager lives in a Tokio mutex shared with the node backend because
/// `MeshManager` is `Send` but not `Sync` (libmdns keeps a non-`Sync` handle).
pub struct ManagerPacketIo {
    manager: Arc<tokio::sync::Mutex<Option<MeshManager>>>,
    receiver: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
}

impl ManagerPacketIo {
    /// Build the adapter from the shared manager slot and the bounded receive
    /// channel returned by [`MeshManager::take_packet_receiver`].
    pub fn new(
        manager: Arc<tokio::sync::Mutex<Option<MeshManager>>>,
        receiver: mpsc::Receiver<Vec<u8>>,
    ) -> Self {
        Self {
            manager,
            receiver: tokio::sync::Mutex::new(receiver),
        }
    }
}

#[async_trait]
impl TunnelIo for ManagerPacketIo {
    async fn send_packet(&self, packet: &[u8]) -> Result<(), PumpError> {
        let manager = self.manager.lock().await;
        let manager = manager.as_ref().ok_or_else(|| {
            PumpError::tunnel_send("WireGuard manager is stopped; packet cannot be encrypted")
        })?;
        manager
            .send_ipv4(packet)
            .await
            .map(|_| ())
            .map_err(|error| PumpError::tunnel_send(error.to_string()))
    }

    async fn recv_packet(&self, buffer: &mut [u8]) -> Result<usize, PumpError> {
        let packet = self
            .receiver
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| PumpError::tunnel_receive("WireGuard packet channel closed"))?;
        if packet.len() > buffer.len() {
            return Err(PumpError::packet_too_large(packet.len(), buffer.len()));
        }
        buffer[..packet.len()].copy_from_slice(&packet);
        Ok(packet.len())
    }
}

#[derive(Default)]
struct Counters {
    tun_read_packets: AtomicU64,
    tun_read_bytes: AtomicU64,
    tun_to_tunnel_packets: AtomicU64,
    tun_to_tunnel_bytes: AtomicU64,
    tunnel_received_packets: AtomicU64,
    tunnel_received_bytes: AtomicU64,
    tunnel_to_tun_packets: AtomicU64,
    tunnel_to_tun_bytes: AtomicU64,
    dropped_tun_to_tunnel: AtomicU64,
    dropped_tunnel_to_tun: AtomicU64,
    tun_read_errors: AtomicU64,
    tun_write_errors: AtomicU64,
    tunnel_send_errors: AtomicU64,
    tunnel_receive_errors: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> PumpStats {
        PumpStats {
            tun_read_packets: self.tun_read_packets.load(Ordering::Relaxed),
            tun_read_bytes: self.tun_read_bytes.load(Ordering::Relaxed),
            tun_to_tunnel_packets: self.tun_to_tunnel_packets.load(Ordering::Relaxed),
            tun_to_tunnel_bytes: self.tun_to_tunnel_bytes.load(Ordering::Relaxed),
            tunnel_received_packets: self.tunnel_received_packets.load(Ordering::Relaxed),
            tunnel_received_bytes: self.tunnel_received_bytes.load(Ordering::Relaxed),
            tunnel_to_tun_packets: self.tunnel_to_tun_packets.load(Ordering::Relaxed),
            tunnel_to_tun_bytes: self.tunnel_to_tun_bytes.load(Ordering::Relaxed),
            dropped_tun_to_tunnel: self.dropped_tun_to_tunnel.load(Ordering::Relaxed),
            dropped_tunnel_to_tun: self.dropped_tunnel_to_tun.load(Ordering::Relaxed),
            tun_read_errors: self.tun_read_errors.load(Ordering::Relaxed),
            tun_write_errors: self.tun_write_errors.load(Ordering::Relaxed),
            tunnel_send_errors: self.tunnel_send_errors.load(Ordering::Relaxed),
            tunnel_receive_errors: self.tunnel_receive_errors.load(Ordering::Relaxed),
        }
    }
}

fn add(counter: &AtomicU64, value: u64) {
    counter.fetch_add(value, Ordering::Relaxed);
}

struct Shared {
    tun: Arc<dyn TunIo>,
    tunnel: Arc<dyn TunnelIo>,
    config: PumpConfig,
    counters: Counters,
    state: Mutex<PumpState>,
    error: Mutex<Option<PumpError>>,
    stop: watch::Sender<bool>,
    failure: watch::Sender<Option<PumpError>>,
    /// Keeps the watch channels open even before workers/monitors subscribe, so
    /// an early failure cannot be lost.
    _stop_keepalive: watch::Receiver<bool>,
    _failure_keepalive: watch::Receiver<Option<PumpError>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    start_lock: Mutex<()>,
}

impl Shared {
    fn state(&self) -> PumpState {
        *self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn set_state(&self, state: PumpState) {
        *self.state.lock().unwrap_or_else(|error| error.into_inner()) = state;
    }

    fn error(&self) -> Option<PumpError> {
        self.error
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn set_error(&self, error: Option<PumpError>) {
        *self.error.lock().unwrap_or_else(|value| value.into_inner()) = error;
    }

    fn fail(&self, error: PumpError) {
        if self.error().is_none() {
            self.set_error(Some(error.clone()));
        }
        let _ = self.stop.send(true);
        let _ = self.failure.send(self.error());
        self.set_state(PumpState::Failed);
    }
}

async fn wait_stop(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

/// Bounded TUN <-> WireGuard packet pump. Cloneable handle; workers are owned by
/// the shared inner state.
#[derive(Clone)]
pub struct PacketPump {
    inner: Arc<Shared>,
}

impl PacketPump {
    pub fn new(tun: Arc<dyn TunIo>, tunnel: Arc<dyn TunnelIo>, config: PumpConfig) -> Self {
        let (stop, stop_keepalive) = watch::channel(false);
        let (failure, failure_keepalive) = watch::channel(None);
        Self {
            inner: Arc::new(Shared {
                tun,
                tunnel,
                config,
                counters: Counters::default(),
                state: Mutex::new(PumpState::Stopped),
                error: Mutex::new(None),
                stop,
                failure,
                _stop_keepalive: stop_keepalive,
                _failure_keepalive: failure_keepalive,
                tasks: Mutex::new(Vec::new()),
                start_lock: Mutex::new(()),
            }),
        }
    }

    pub fn config(&self) -> PumpConfig {
        self.inner.config
    }

    pub fn state(&self) -> PumpState {
        self.inner.state()
    }

    pub fn error(&self) -> Option<PumpError> {
        self.inner.error()
    }

    pub fn stats(&self) -> PumpStats {
        self.inner.counters.snapshot()
    }

    pub fn status(&self, interface: Option<String>) -> PumpStatus {
        PumpStatus {
            configured: true,
            state: self.state(),
            interface,
            queue_capacity: self.inner.config.queue_capacity,
            stats: self.stats(),
            last_error: self.error(),
        }
    }

    /// Start both directions. Idempotent while running. A previous failure is
    /// cleared only by [`PacketPump::stop`] followed by `start`.
    pub fn start(&self) -> Result<(), PumpError> {
        let _guard = self
            .inner
            .start_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());

        if self.state() == PumpState::Running {
            return Ok(());
        }
        if self.state() == PumpState::Starting {
            return Err(PumpError::invalid("packet pump is already starting"));
        }
        if let Some(error) = self.error() {
            return Err(error);
        }
        let config = self.inner.config.validate()?;

        self.inner.set_error(None);
        self.inner.set_state(PumpState::Starting);
        let _ = self.inner.stop.send(false);
        let _ = self.inner.failure.send(None);

        let capacity = config.queue_capacity;
        let (tun_to_tunnel_tx, tun_to_tunnel_rx) = mpsc::channel(capacity);
        let (tunnel_to_tun_tx, tunnel_to_tun_rx) = mpsc::channel(capacity);

        // Mark running before spawning so an immediately failing worker cannot
        // be overwritten by the successful start path.
        self.inner.set_state(PumpState::Running);
        let mut tasks = Vec::with_capacity(4);
        tasks.push(tokio::spawn(tun_reader(
            Arc::clone(&self.inner),
            tun_to_tunnel_tx,
        )));
        tasks.push(tokio::spawn(tunnel_sender(
            Arc::clone(&self.inner),
            tun_to_tunnel_rx,
        )));
        tasks.push(tokio::spawn(tunnel_reader(
            Arc::clone(&self.inner),
            tunnel_to_tun_tx,
        )));
        tasks.push(tokio::spawn(tun_writer(
            Arc::clone(&self.inner),
            tunnel_to_tun_rx,
        )));

        *self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = tasks;
        debug!(
            queue_capacity = config.queue_capacity,
            packet_buffer = config.packet_buffer,
            "mesh packet pump started"
        );
        Ok(())
    }

    /// Stop both directions and abort any in-flight worker future. Safe to call
    /// repeatedly and after a failure.
    pub fn stop(&self) {
        let _guard = self
            .inner
            .start_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _ = self.inner.stop.send(true);
        let tasks = std::mem::take(
            &mut *self
                .inner
                .tasks
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        for task in tasks {
            task.abort();
        }
        self.inner.set_state(PumpState::Stopped);
        self.inner.set_error(None);
    }

    /// Wait until a worker fails (returns the typed error) or the pump stops
    /// normally (returns `None`). Used by the node service to tear down the
    /// dataplane/tunnel after an asynchronous pump failure.
    pub async fn wait_failed(&self) -> Option<PumpError> {
        let mut failure = self.inner.failure.subscribe();
        loop {
            if let Some(error) = failure.borrow().clone() {
                return Some(error);
            }
            if self.state() != PumpState::Running {
                return None;
            }
            if failure.changed().await.is_err() {
                return None;
            }
        }
    }
}

async fn tun_reader(shared: Arc<Shared>, queue: mpsc::Sender<Vec<u8>>) {
    let mut buffer = vec![0u8; shared.config.packet_buffer];
    let mut stop = shared.stop.subscribe();
    loop {
        let result = tokio::select! {
            _ = wait_stop(&mut stop) => break,
            result = shared.tun.read_packet(&mut buffer) => result,
        };
        match result {
            Ok(0) => {
                tokio::task::yield_now().await;
            }
            Ok(read) => {
                add(&shared.counters.tun_read_packets, 1);
                add(&shared.counters.tun_read_bytes, read as u64);
                match queue.try_send(buffer[..read].to_vec()) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        add(&shared.counters.dropped_tun_to_tunnel, 1);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
            Err(error) => {
                add(&shared.counters.tun_read_errors, 1);
                shared.fail(error);
                break;
            }
        }
    }
}

async fn tunnel_sender(shared: Arc<Shared>, mut queue: mpsc::Receiver<Vec<u8>>) {
    let mut stop = shared.stop.subscribe();
    loop {
        let packet = tokio::select! {
            _ = wait_stop(&mut stop) => break,
            packet = queue.recv() => packet,
        };
        let Some(packet) = packet else {
            break;
        };
        match shared.tunnel.send_packet(&packet).await {
            Ok(()) => {
                add(&shared.counters.tun_to_tunnel_packets, 1);
                add(&shared.counters.tun_to_tunnel_bytes, packet.len() as u64);
            }
            Err(error) => {
                add(&shared.counters.tunnel_send_errors, 1);
                shared.fail(error);
                break;
            }
        }
    }
}

async fn tunnel_reader(shared: Arc<Shared>, queue: mpsc::Sender<Vec<u8>>) {
    let mut buffer = vec![0u8; shared.config.packet_buffer];
    let mut stop = shared.stop.subscribe();
    loop {
        let result = tokio::select! {
            _ = wait_stop(&mut stop) => break,
            result = shared.tunnel.recv_packet(&mut buffer) => result,
        };
        match result {
            Ok(0) => {
                tokio::task::yield_now().await;
            }
            Ok(read) => {
                add(&shared.counters.tunnel_received_packets, 1);
                add(&shared.counters.tunnel_received_bytes, read as u64);
                match queue.try_send(buffer[..read].to_vec()) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        add(&shared.counters.dropped_tunnel_to_tun, 1);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
            Err(error) => {
                add(&shared.counters.tunnel_receive_errors, 1);
                shared.fail(error);
                break;
            }
        }
    }
}

async fn tun_writer(shared: Arc<Shared>, mut queue: mpsc::Receiver<Vec<u8>>) {
    let mut stop = shared.stop.subscribe();
    loop {
        let packet = tokio::select! {
            _ = wait_stop(&mut stop) => break,
            packet = queue.recv() => packet,
        };
        let Some(packet) = packet else {
            break;
        };
        let mut offset = 0usize;
        while offset < packet.len() {
            let result = tokio::select! {
                _ = wait_stop(&mut stop) => return,
                result = shared.tun.write_packet(&packet[offset..]) => result,
            };
            match result {
                Ok(0) => {
                    add(&shared.counters.tun_write_errors, 1);
                    shared.fail(PumpError::tun_write(format!(
                        "TUN write returned 0 after {offset} of {} bytes",
                        packet.len()
                    )));
                    return;
                }
                Ok(written) => offset += written.min(packet.len() - offset),
                Err(error) => {
                    add(&shared.counters.tun_write_errors, 1);
                    shared.fail(error);
                    return;
                }
            }
        }
        add(&shared.counters.tunnel_to_tun_packets, 1);
        add(&shared.counters.tunnel_to_tun_bytes, packet.len() as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::Duration;
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
    use tokio::sync::{Mutex as TokioMutex, Semaphore};
    use tokio::time::timeout;

    struct FakeTun {
        inbound: TokioMutex<UnboundedReceiver<Vec<u8>>>,
        outbound: UnboundedSender<Vec<u8>>,
        write_gate: Option<Arc<Semaphore>>,
        write_chunk: usize,
        read_error: Mutex<Option<PumpError>>,
        write_error: Mutex<Option<PumpError>>,
        partial: Mutex<VecDeque<usize>>,
    }

    impl FakeTun {
        fn new() -> (
            Arc<Self>,
            UnboundedSender<Vec<u8>>,
            UnboundedReceiver<Vec<u8>>,
        ) {
            let (in_tx, in_rx) = mpsc::unbounded_channel();
            let (out_tx, out_rx) = mpsc::unbounded_channel();
            (
                Arc::new(Self {
                    inbound: TokioMutex::new(in_rx),
                    outbound: out_tx,
                    write_gate: None,
                    write_chunk: 0,
                    read_error: Mutex::new(None),
                    write_error: Mutex::new(None),
                    partial: Mutex::new(VecDeque::new()),
                }),
                in_tx,
                out_rx,
            )
        }

        fn with_write_gate(
            gate: Arc<Semaphore>,
        ) -> (
            Arc<Self>,
            UnboundedSender<Vec<u8>>,
            UnboundedReceiver<Vec<u8>>,
        ) {
            let (tun, tx, rx) = Self::new();
            let mut replacement = Arc::try_unwrap(tun).ok().expect("fresh fake tun");
            replacement.write_gate = Some(gate);
            (Arc::new(replacement), tx, rx)
        }

        fn with_write_chunk(
            chunk: usize,
        ) -> (
            Arc<Self>,
            UnboundedSender<Vec<u8>>,
            UnboundedReceiver<Vec<u8>>,
        ) {
            let (tun, tx, rx) = Self::new();
            let mut replacement = Arc::try_unwrap(tun).ok().expect("fresh fake tun");
            replacement.write_chunk = chunk;
            (Arc::new(replacement), tx, rx)
        }
    }

    #[async_trait]
    impl TunIo for FakeTun {
        async fn read_packet(&self, buffer: &mut [u8]) -> Result<usize, PumpError> {
            if let Some(error) = self
                .read_error
                .lock()
                .unwrap_or_else(|value| value.into_inner())
                .clone()
            {
                return Err(error);
            }
            let mut packet = self
                .inbound
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| PumpError::tun_read("fake TUN inbound channel closed"))?;
            let limit = self
                .partial
                .lock()
                .unwrap_or_else(|value| value.into_inner())
                .pop_front()
                .unwrap_or(packet.len());
            packet.truncate(limit.min(packet.len()));
            let length = packet.len();
            buffer[..length].copy_from_slice(&packet);
            Ok(length)
        }

        async fn write_packet(&self, packet: &[u8]) -> Result<usize, PumpError> {
            if let Some(error) = self
                .write_error
                .lock()
                .unwrap_or_else(|value| value.into_inner())
                .clone()
            {
                return Err(error);
            }
            if let Some(gate) = &self.write_gate {
                let _permit = gate
                    .acquire()
                    .await
                    .map_err(|_| PumpError::tun_write("write gate closed"))?;
            }
            let length = if self.write_chunk == 0 {
                packet.len()
            } else {
                packet.len().min(self.write_chunk)
            };
            if length == 0 {
                return Ok(0);
            }
            let _ = self.outbound.send(packet[..length].to_vec());
            Ok(length)
        }
    }

    struct FakeTunnel {
        sent: UnboundedSender<Vec<u8>>,
        incoming: TokioMutex<UnboundedReceiver<Vec<u8>>>,
        send_gate: Option<Arc<Semaphore>>,
        send_error: Mutex<Option<PumpError>>,
        receive_error: Mutex<Option<PumpError>>,
    }

    impl FakeTunnel {
        fn new() -> (
            Arc<Self>,
            UnboundedReceiver<Vec<u8>>,
            UnboundedSender<Vec<u8>>,
        ) {
            let (sent_tx, sent_rx) = mpsc::unbounded_channel();
            let (in_tx, in_rx) = mpsc::unbounded_channel();
            (
                Arc::new(Self {
                    sent: sent_tx,
                    incoming: TokioMutex::new(in_rx),
                    send_gate: None,
                    send_error: Mutex::new(None),
                    receive_error: Mutex::new(None),
                }),
                sent_rx,
                in_tx,
            )
        }

        fn with_send_gate(
            gate: Arc<Semaphore>,
        ) -> (
            Arc<Self>,
            UnboundedReceiver<Vec<u8>>,
            UnboundedSender<Vec<u8>>,
        ) {
            let (tunnel, sent, incoming) = Self::new();
            let mut replacement = Arc::try_unwrap(tunnel).ok().expect("fresh fake tunnel");
            replacement.send_gate = Some(gate);
            (Arc::new(replacement), sent, incoming)
        }
    }

    #[async_trait]
    impl TunnelIo for FakeTunnel {
        async fn send_packet(&self, packet: &[u8]) -> Result<(), PumpError> {
            if let Some(error) = self
                .send_error
                .lock()
                .unwrap_or_else(|value| value.into_inner())
                .clone()
            {
                return Err(error);
            }
            if let Some(gate) = &self.send_gate {
                let _permit = gate
                    .acquire()
                    .await
                    .map_err(|_| PumpError::tunnel_send("send gate closed"))?;
            }
            self.sent
                .send(packet.to_vec())
                .map_err(|_| PumpError::tunnel_send("fake tunnel sent channel closed"))
        }

        async fn recv_packet(&self, buffer: &mut [u8]) -> Result<usize, PumpError> {
            if let Some(error) = self
                .receive_error
                .lock()
                .unwrap_or_else(|value| value.into_inner())
                .clone()
            {
                return Err(error);
            }
            let packet = self
                .incoming
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| PumpError::tunnel_receive("fake tunnel input closed"))?;
            if packet.len() > buffer.len() {
                return Err(PumpError::packet_too_large(packet.len(), buffer.len()));
            }
            buffer[..packet.len()].copy_from_slice(&packet);
            Ok(packet.len())
        }
    }

    fn packet(tag: u8, length: usize) -> Vec<u8> {
        let mut packet = vec![tag; length];
        packet[0] = 0x45;
        packet
    }

    async fn wait_until(mut predicate: impl FnMut() -> bool) {
        timeout(Duration::from_secs(3), async {
            loop {
                if predicate() {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("condition was not reached before timeout");
    }

    fn config(queue_capacity: usize) -> PumpConfig {
        PumpConfig {
            queue_capacity,
            packet_buffer: 2048,
        }
    }

    #[tokio::test]
    async fn tun_to_tunnel_and_tunnel_to_tun_round_trip() {
        let (tun, tun_inject, mut tun_out) = FakeTun::new();
        let (tunnel, mut tunnel_sent, tunnel_inject) = FakeTunnel::new();
        let pump = PacketPump::new(tun, tunnel, config(8));
        pump.start().unwrap();
        assert_eq!(pump.state(), PumpState::Running);

        tun_inject.send(packet(0x11, 64)).unwrap();
        let sent = timeout(Duration::from_secs(2), tunnel_sent.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sent, packet(0x11, 64));

        tunnel_inject.send(packet(0x22, 80)).unwrap();
        let written = timeout(Duration::from_secs(2), tun_out.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(written, packet(0x22, 80));

        wait_until(|| pump.stats().tun_to_tunnel_packets == 1).await;
        wait_until(|| pump.stats().tunnel_to_tun_packets == 1).await;
        let stats = pump.stats();
        assert_eq!(stats.tun_read_packets, 1);
        assert_eq!(stats.tun_read_bytes, 64);
        assert_eq!(stats.tun_to_tunnel_bytes, 64);
        assert_eq!(stats.tunnel_received_packets, 1);
        assert_eq!(stats.tunnel_received_bytes, 80);
        assert_eq!(stats.tunnel_to_tun_bytes, 80);
        pump.stop();
    }

    #[tokio::test]
    async fn short_tun_read_is_one_packet() {
        let (tun, tun_inject, _tun_out) = FakeTun::new();
        tun.partial
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back(10);
        let (tunnel, mut tunnel_sent, _tunnel_inject) = FakeTunnel::new();
        let pump = PacketPump::new(tun, tunnel, config(8));
        pump.start().unwrap();

        tun_inject.send(packet(0x33, 40)).unwrap();
        let sent = timeout(Duration::from_secs(2), tunnel_sent.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sent.len(), 10, "short TUN read is one short packet");
        pump.stop();
    }

    #[tokio::test]
    async fn partial_tun_writes_are_retried() {
        let (tun, _inject, mut tun_out) = FakeTun::with_write_chunk(7);
        let (tunnel, _sent, tunnel_inject) = FakeTunnel::new();
        let pump = PacketPump::new(tun, tunnel, config(8));
        pump.start().unwrap();
        tunnel_inject.send(packet(0x44, 31)).unwrap();

        let mut received = Vec::new();
        while received.len() < 31 {
            let chunk = timeout(Duration::from_secs(2), tun_out.recv())
                .await
                .unwrap()
                .unwrap();
            received.extend_from_slice(&chunk);
        }
        assert_eq!(received, packet(0x44, 31));
        wait_until(|| pump.stats().tunnel_to_tun_packets == 1).await;
        pump.stop();
    }

    #[tokio::test]
    async fn tun_to_tunnel_queue_overflow_drops_and_counts() {
        let (tun, tun_inject, _tun_out) = FakeTun::new();
        let gate = Arc::new(Semaphore::new(0));
        let (tunnel, mut sent, _inject) = FakeTunnel::with_send_gate(gate.clone());
        let pump = PacketPump::new(tun, tunnel, config(1));
        pump.start().unwrap();

        // The sender is gated, so the second packet cannot be queued.
        for tag in 0..4 {
            tun_inject.send(packet(tag, 32)).unwrap();
        }
        wait_until(|| pump.stats().dropped_tun_to_tunnel > 0).await;
        gate.add_permits(8);
        drop(gate);
        let first = timeout(Duration::from_secs(2), sent.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first[0], 0x45);
        pump.stop();
    }

    #[tokio::test]
    async fn tunnel_to_tun_queue_overflow_drops_and_counts() {
        let gate = Arc::new(Semaphore::new(0));
        let (tun, _inject, mut out) = FakeTun::with_write_gate(gate.clone());
        let (tunnel, _sent, tunnel_inject) = FakeTunnel::new();
        let pump = PacketPump::new(tun, tunnel, config(1));
        pump.start().unwrap();

        for tag in 0..4 {
            tunnel_inject.send(packet(tag, 32)).unwrap();
        }
        wait_until(|| pump.stats().dropped_tunnel_to_tun > 0).await;
        gate.add_permits(8);
        drop(gate);
        let first = timeout(Duration::from_secs(2), out.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first[0], 0x45);
        pump.stop();
    }

    #[tokio::test]
    async fn shutdown_is_idempotent_and_stops_both_directions() {
        let (tun, tun_inject, mut tun_out) = FakeTun::new();
        let (tunnel, mut sent, tunnel_inject) = FakeTunnel::new();
        let pump = PacketPump::new(tun, tunnel, config(4));
        pump.start().unwrap();
        pump.stop();
        assert_eq!(pump.state(), PumpState::Stopped);
        pump.stop();
        assert_eq!(pump.state(), PumpState::Stopped);

        // Nothing should move after shutdown even if a test injects data.
        tun_inject.send(packet(0x55, 20)).unwrap();
        let _ = tunnel_inject.send(packet(0x66, 20));
        assert!(timeout(Duration::from_millis(50), sent.recv())
            .await
            .is_err());
        assert!(timeout(Duration::from_millis(50), tun_out.recv())
            .await
            .is_err());
        assert!(pump.wait_failed().await.is_none());
    }

    #[tokio::test]
    async fn tunnel_errors_are_typed_and_stop_the_pump() {
        let (tun, tun_inject, _out) = FakeTun::new();
        let (tunnel, _sent, _inject) = FakeTunnel::new();
        *tunnel
            .send_error
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(PumpError::tunnel_send("encrypt failed"));
        let pump = PacketPump::new(tun, tunnel.clone(), config(4));
        pump.start().unwrap();
        tun_inject.send(packet(0x77, 32)).unwrap();

        let error = timeout(Duration::from_secs(2), pump.wait_failed())
            .await
            .unwrap()
            .expect("pump should report the tunnel error");
        assert_eq!(error.code, "mesh.pump-tunnel-send");
        assert_eq!(pump.state(), PumpState::Failed);
        assert!(pump.stats().tunnel_send_errors >= 1);
        pump.stop();
    }

    #[tokio::test]
    async fn tunnel_receive_errors_are_typed_and_stop_the_pump() {
        let (tun, _inject, _out) = FakeTun::new();
        let (tunnel, _sent, _inject) = FakeTunnel::new();
        *tunnel
            .receive_error
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(PumpError::tunnel_receive("decrypt failed"));
        let pump = PacketPump::new(tun, tunnel, config(4));
        pump.start().unwrap();

        let error = timeout(Duration::from_secs(2), pump.wait_failed())
            .await
            .unwrap()
            .expect("pump should report the tunnel receive error");
        assert_eq!(error.code, "mesh.pump-tunnel-receive");
        assert_eq!(pump.state(), PumpState::Failed);
        assert!(pump.stats().tunnel_receive_errors >= 1);
        pump.stop();
    }

    #[tokio::test]
    async fn tun_errors_are_typed_and_stop_the_pump() {
        let (tun, _inject, _out) = FakeTun::new();
        *tun.read_error
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(PumpError::tun_read("device disappeared"));
        let (tunnel, _sent, _inject) = FakeTunnel::new();
        let pump = PacketPump::new(tun, tunnel, config(4));
        pump.start().unwrap();

        let error = timeout(Duration::from_secs(2), pump.wait_failed())
            .await
            .unwrap()
            .expect("pump should report the TUN error");
        assert_eq!(error.code, "mesh.pump-tun-read");
        assert_eq!(pump.state(), PumpState::Failed);
        assert!(pump.stats().tun_read_errors >= 1);
        pump.stop();
    }

    #[tokio::test]
    #[ignore = "requires root, /dev/net/tun and ip; set OZMA_MESH_LIVE=1"]
    async fn live_system_tun_attaches_to_the_dataplane_link() {
        if std::env::var("OZMA_MESH_LIVE").ok().as_deref() != Some("1") {
            return;
        }
        let config = crate::dataplane::DataplaneConfig::new(
            "ozmatest-pmp",
            std::net::Ipv4Addr::new(10, 253, 78, 1),
            24,
            1420,
        );
        let dataplane = crate::dataplane::MeshDataplane::system(config);
        dataplane.start().expect("live dataplane start");
        let provider = SystemTunProvider::new();
        let tun = provider
            .open(&dataplane.config().interface)
            .await
            .expect("persistent TUN attach");
        drop(tun);
        dataplane.stop().unwrap();
        assert_eq!(
            dataplane.status().state,
            crate::dataplane::DataplaneState::Stopped
        );
    }

    #[tokio::test]
    async fn invalid_bounds_are_typed_and_never_start() {
        let (tun, _inject, _out) = FakeTun::new();
        let (tunnel, _sent, _inject) = FakeTunnel::new();
        let pump = PacketPump::new(
            tun,
            tunnel,
            PumpConfig {
                queue_capacity: 0,
                packet_buffer: 2048,
            },
        );
        let error = pump.start().unwrap_err();
        assert_eq!(error.code, "mesh.pump-invalid");
        assert_eq!(pump.state(), PumpState::Stopped);
        assert_eq!(pump.stats(), PumpStats::default());
    }
}
