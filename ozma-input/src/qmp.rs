// SPDX-License-Identifier: AGPL-3.0-only
//! QEMU QMP input sink: normalized events to `input-send-event`.
//!
//! QEMU can inject keyboard and pointer input over QMP without a USB gadget or
//! `/dev/uinput`. This sink translates [`InputEvent`] values into
//! [`QmpInputEvent`] batches, queues them on an unbounded channel, and leaves
//! the async socket work to a background task. The [`InputSink`] boundary is
//! synchronous and non-blocking: [`QmpSink::submit_report`] only locks the
//! held-state mutex and calls [`tokio::sync::mpsc::UnboundedSender::send`].
//!
//! # Translation
//!
//! * Keyboard usages are mapped through [`ozma_qmp::hid_to_qcode`]. Client
//!   [`KeyModifiers`] bits are held as virtual modifier keys (`0xE0..=0xE7`)
//!   and diffed exactly like [`crate::UinputSink`]. Explicit modifier usages
//!   therefore share the same table and cannot leave a modifier stuck.
//! * Absolute pointer events become `abs` events for `x` and `y` in QEMU's
//!   `0..=32767` range. Coordinates are mapped with [`AbsoluteMapping`], which
//!   clamps out-of-range values to the nearest edge.
//! * Relative pointer events become `rel` events. Zero deltas are omitted.
//! * Buttons map to QEMU's button names: left/right/middle plus `side` for
//!   [`PointerButton::Back`] and `extra` for [`PointerButton::Forward`].
//! * QMP has no wheel axis. A vertical wheel delta becomes one
//!   `wheel-up`/`wheel-down` press+release pair per notch, matching the
//!   translation in [`ozma_qmp::MouseReportState`]. Positive `dy` scrolls up.
//!   Horizontal wheel motion is rejected: QEMU's `input-send-event` has no
//!   horizontal wheel button or axis, and silently turning it into vertical
//!   scrolling would be wrong.
//! * Touch, pen, and clipboard events are rejected with
//!   [`codes::UNSUPPORTED`](crate::codes::UNSUPPORTED). QMP's input event model
//!   has no multitouch contact, tablet pressure/tilt, or clipboard transport.
//! * [`InputEvent::ReleaseAll`] emits key-ups for every held key/modifier and
//!   button-ups for every held button. Local state is cleared even if the
//!   channel send fails, so a stopped background task cannot leave the sink
//!   believing keys are still down.
//!
//! # Failure semantics
//!
//! A normal event is planned on a clone of the held state and committed only
//! after the batch enters the channel. A closed channel returns a typed
//! [`ApiError`] and leaves state unchanged, matching the write semantics of the
//! gadget and uinput sinks. [`InputSink::release_all`] is the exception: like
//! those sinks it clears state even when the final write fails.
//!
//! [`QmpSink::spawn`] attempts one reconnect after a send failure and keeps
//! draining the channel. It does not retry the batch that failed: an event
//! that could not be delivered is logged rather than replayed out of order.
//! Indefinite retry belongs to a supervisor, not to the event path.
//!
//! # Socket precedence
//!
//! * [`QmpSink::spawn_with_shared`] is the single-control-socket mode: input
//!   shares the node's one [`ozma_qmp::SharedQmpClient`] with power, capture,
//!   and snapshots, so no second QMP client is opened.
//! * [`QmpSink::spawn_dedicated_input`] is the provisioned dual-socket mode:
//!   `input-send-event` writes go to the dedicated `--qmp-input-socket` with
//!   fire-and-forget semantics and never touch the control socket.
//! * [`QmpSink::spawn`] keeps the original owned-client behavior (single or
//!   dual config) for callers that manage their own QMP lifecycle.
//!
//! A caller that has a dedicated input socket should prefer it for input only;
//! otherwise it should pass the shared control client.

use std::collections::BTreeSet;
use std::sync::Mutex;

use ozma_protocol::input::{InputEvent, KeyModifiers, PointerButton};
use ozma_protocol::ApiError;
use ozma_qmp::{hid_to_qcode, QmpClient, QmpConfig, QmpInputEvent, SharedQmpClient};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tracing::{debug, warn};

use crate::error;
use crate::report::{pointer_button_bit, AbsoluteMapping, AbsolutePoint};
use crate::sink::{lock, InputSink, SubmitReport};
use crate::state::HeldInput;

const MODIFIER_USAGE_START: u8 = 0xE0;
const MODIFIER_USAGE_END: u8 = 0xE7;

const BACK_BUTTON: &str = "side";
const FORWARD_BUTTON: &str = "extra";

/// Background task returned by [`QmpSink::spawn`].
pub type QmpSinkHandle = tokio::task::JoinHandle<()>;

/// Owns the keys, modifiers, buttons, and last pointer position for a QMP
/// sink. The state machine mirrors the uinput and gadget sinks but is
/// transactionally cloned by [`QmpSink::submit_report`].
#[derive(Clone, Debug)]
struct QmpSinkState {
    /// Non-modifier HID usages currently held.
    keys: BTreeSet<u8>,
    /// Modifier bits set by explicit `0xE0..=0xE7` key events.
    explicit_modifiers: u8,
    /// Modifier bits carried by the last non-modifier key event.
    client_modifiers: u8,
    /// Effective pointer button bitmask.
    buttons: u8,
    /// Mapping from event coordinates to absolute report space.
    mapping: AbsoluteMapping,
    /// Last absolute position, diagnostic only.
    position: AbsolutePoint,
    positioned: bool,
}

impl QmpSinkState {
    fn new(mapping: AbsoluteMapping) -> Self {
        Self {
            keys: BTreeSet::new(),
            explicit_modifiers: 0,
            client_modifiers: 0,
            buttons: 0,
            mapping,
            position: AbsolutePoint::CENTER,
            positioned: false,
        }
    }

    fn modifiers(&self) -> u8 {
        self.explicit_modifiers | self.client_modifiers
    }

    fn down_usages(&self) -> BTreeSet<u8> {
        let mut usages = self.keys.clone();
        let modifiers = self.modifiers();
        for bit in 0..8u8 {
            if modifiers & (1 << bit) != 0 {
                usages.insert(MODIFIER_USAGE_START + bit);
            }
        }
        usages
    }

    fn snapshot(&self) -> HeldInput {
        HeldInput {
            keys: self.keys.clone(),
            modifiers: self.modifiers(),
            buttons: self.buttons,
            contacts: BTreeSet::new(),
            pen_down: false,
            pointer: if self.positioned {
                Some(self.position)
            } else {
                None
            },
        }
    }

    fn plan(&mut self, event: &InputEvent) -> Result<Vec<QmpInputEvent>, ApiError> {
        match event {
            InputEvent::Key {
                code,
                down,
                modifiers,
            } => self.key(*code, *down, *modifiers),
            InputEvent::PointerAbsolute { x, y, .. } => Ok(self.absolute(*x, *y)),
            InputEvent::PointerRelative { dx, dy } => Ok(self.relative(*dx, *dy)),
            InputEvent::Button { button, down } => Ok(self.button(*button, *down)),
            InputEvent::Wheel { dx, dy } => self.wheel(*dx, *dy),
            InputEvent::Touch { .. } => Err(error::unsupported_event(
                "touch",
                "QMP input-send-event has no touch contact or multitouch representation",
            )),
            InputEvent::Pen { .. } => Err(error::unsupported_event(
                "pen",
                "QMP input-send-event has no tablet pressure/tilt representation",
            )),
            InputEvent::Clipboard { .. } => Err(error::unsupported_event(
                "clipboard",
                "The input sink does not transport clipboard data",
            )),
            InputEvent::ReleaseAll => Ok(self.release_all_planned()),
        }
    }

    fn key(
        &mut self,
        code: u32,
        down: bool,
        modifiers: KeyModifiers,
    ) -> Result<Vec<QmpInputEvent>, ApiError> {
        if code == 0 || code > u32::from(u8::MAX) {
            return Err(error::invalid_key(code));
        }
        let usage = code as u8;
        if hid_to_qcode(usage).is_none() {
            // Reject before mutating so a caller can retry or fall back.
            return Err(error::unmapped_qmp_key(usage));
        }

        let before = self.down_usages();
        if is_modifier_usage(usage) {
            let bit = 1u8 << (usage - MODIFIER_USAGE_START);
            if down {
                self.explicit_modifiers |= bit;
            } else {
                self.explicit_modifiers &= !bit;
            }
        } else {
            // Match Router and UinputSink: a non-modifier key event carries the
            // client's current modifier mask, including on key-up.
            self.client_modifiers = modifiers.0;
            if down {
                self.keys.insert(usage);
            } else {
                self.keys.remove(&usage);
            }
        }

        Ok(qmp_key_diff(&before, &self.down_usages()))
    }

    fn absolute(&mut self, x: i32, y: i32) -> Vec<QmpInputEvent> {
        let point = self.mapping.map(x, y);
        self.position = point;
        self.positioned = true;
        vec![
            QmpInputEvent::abs("x", u32::from(point.x)),
            QmpInputEvent::abs("y", u32::from(point.y)),
        ]
    }

    fn relative(&mut self, dx: i32, dy: i32) -> Vec<QmpInputEvent> {
        let mut events = Vec::with_capacity(2);
        if dx != 0 {
            events.push(QmpInputEvent::rel("x", dx));
        }
        if dy != 0 {
            events.push(QmpInputEvent::rel("y", dy));
        }
        events
    }

    fn button(&mut self, button: PointerButton, down: bool) -> Vec<QmpInputEvent> {
        let bit = pointer_button_bit(button);
        if down {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        vec![QmpInputEvent::button(pointer_button_name(button), down)]
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<Vec<QmpInputEvent>, ApiError> {
        if dx != 0 {
            return Err(error::unsupported_event(
                "wheel",
                "QMP input-send-event has no horizontal wheel button or axis",
            ));
        }

        let mut events = Vec::with_capacity((dy.unsigned_abs() as usize).saturating_mul(2));
        if dy > 0 {
            for _ in 0..dy {
                events.push(QmpInputEvent::button("wheel-up", true));
                events.push(QmpInputEvent::button("wheel-up", false));
            }
        } else if dy < 0 {
            for _ in 0..-dy {
                events.push(QmpInputEvent::button("wheel-down", true));
                events.push(QmpInputEvent::button("wheel-down", false));
            }
        }
        Ok(events)
    }

    fn release_all_planned(&mut self) -> Vec<QmpInputEvent> {
        let before = self.down_usages();
        let buttons = self.buttons;

        self.keys.clear();
        self.explicit_modifiers = 0;
        self.client_modifiers = 0;
        self.buttons = 0;

        let mut events = qmp_key_diff(&before, &BTreeSet::new());
        for button in [
            PointerButton::Left,
            PointerButton::Right,
            PointerButton::Middle,
            PointerButton::Back,
            PointerButton::Forward,
        ] {
            if buttons & pointer_button_bit(button) != 0 {
                events.push(QmpInputEvent::button(pointer_button_name(button), false));
            }
        }
        events
    }
}

/// QMP sink that queues `input-send-event` batches for a background task.
///
/// Use [`QmpSink::channel`] for tests and custom transports, or
/// [`QmpSink::spawn`] to connect a real QMP client. The sink itself never
/// performs async I/O, so it can be shared through [`crate::SharedSink`] and
/// called from any control task.
#[derive(Debug)]
pub struct QmpSink {
    sender: UnboundedSender<Vec<QmpInputEvent>>,
    state: Mutex<QmpSinkState>,
}

impl QmpSink {
    /// Create a sink and the receiver its sender task should drain.
    ///
    /// This is the testable seam: no socket is opened and no task is spawned.
    pub fn channel() -> (Self, UnboundedReceiver<Vec<QmpInputEvent>>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Self::from_sender(sender), receiver)
    }

    /// Wrap an existing batch sender with the default HID-space mapping.
    pub fn from_sender(sender: UnboundedSender<Vec<QmpInputEvent>>) -> Self {
        Self::with_mapping(sender, AbsoluteMapping::default())
    }

    /// Wrap an existing batch sender with an explicit coordinate mapping.
    ///
    /// The default is [`AbsoluteMapping::hid_space`]; use
    /// [`AbsoluteMapping::new`] when [`InputEvent::PointerAbsolute`] carries
    /// surface pixels instead of report coordinates.
    pub fn with_mapping(
        sender: UnboundedSender<Vec<QmpInputEvent>>,
        mapping: AbsoluteMapping,
    ) -> Self {
        Self {
            sender,
            state: Mutex::new(QmpSinkState::new(mapping)),
        }
    }

    /// Connect a QMP client and spawn the task that drains this sink.
    ///
    /// The returned handle stops the task when aborted. Dropping the sink also
    /// ends the task after the queued batches have been drained. A send
    /// failure is logged and followed by one reconnect attempt; the batch that
    /// failed is not replayed.
    pub async fn spawn(config: QmpConfig) -> Result<(Self, QmpSinkHandle), ApiError> {
        let client = QmpClient::connect(config.clone())
            .await
            .map_err(|error| error::qmp_connect_error(&config, error))?;
        let (sink, receiver) = Self::channel();
        let handle = tokio::spawn(run_input_task(client, receiver));
        Ok((sink, handle))
    }

    /// Share the node's single control client instead of opening another QMP
    /// connection.
    ///
    /// Input batches call `input-send-event` on the shared client while its
    /// command mutex serializes them with power, capture, and snapshots. This is
    /// the correct mode for a VM with one `--qmp-socket` and no
    /// `--qmp-input-socket`.
    pub fn spawn_with_shared(shared: SharedQmpClient) -> (Self, QmpSinkHandle) {
        let (sink, receiver) = Self::channel();
        let handle = tokio::spawn(run_shared_input_task(shared, receiver));
        (sink, handle)
    }

    /// Write input to a dedicated `--qmp-input-socket` only.
    ///
    /// The client is built in dual-socket mode and connects only the input
    /// half, so it never takes a second control client. `input-send-event`
    /// keeps the fire-and-forget behavior of the dedicated socket. A
    /// non-dual config falls back to [`QmpSink::spawn`].
    pub async fn spawn_dedicated_input(
        config: QmpConfig,
    ) -> Result<(Self, QmpSinkHandle), ApiError> {
        if !config.is_dual() {
            return Self::spawn(config).await;
        }

        let client = QmpClient::new(config.clone());
        client
            .connect_input()
            .await
            .map_err(|error| error::qmp_connect_error(&config, error))?;
        let (sink, receiver) = Self::channel();
        let handle = tokio::spawn(run_dedicated_input_task(client, receiver));
        Ok((sink, handle))
    }

    fn dispatch(&self, events: Vec<QmpInputEvent>) -> Result<(), ApiError> {
        if events.is_empty() {
            return Ok(());
        }
        self.sender
            .send(events)
            .map_err(|_| error::qmp_channel_closed())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QmpSinkState> {
        lock(&self.state)
    }
}

impl InputSink for QmpSink {
    fn submit_report(&self, event: &InputEvent) -> Result<SubmitReport, ApiError> {
        let mut state = self.lock();

        if matches!(event, InputEvent::ReleaseAll) {
            // ReleaseAll clears state before the send, so a closed channel
            // cannot leave the sink believing keys are still held.
            let events = state.plan(event)?;
            let written = events.len();
            self.dispatch(events)?;
            return Ok(SubmitReport {
                reports_written: written,
                state_changed: written > 0,
            });
        }

        let mut next = state.clone();
        let events = next.plan(event)?;
        let written = events.len();

        if written == 0 {
            // Commit anyway so a no-op event (a duplicate press or a release
            // of an unheld key) leaves state exactly as planned.
            *state = next;
            return Ok(SubmitReport::noop());
        }

        self.dispatch(events)?;
        *state = next;
        Ok(SubmitReport {
            reports_written: written,
            state_changed: true,
        })
    }

    fn release_all(&self) -> Result<(), ApiError> {
        let mut state = self.lock();
        let events = state.plan(&InputEvent::ReleaseAll)?;
        // State has already been cleared by `plan`; surface a send failure.
        self.dispatch(events)
    }

    fn held(&self) -> HeldInput {
        self.lock().snapshot()
    }
}

/// Diff two usage sets into QMP key events.
///
/// Releases of non-modifiers come before modifier releases, so a shortcut's
/// letter is released while the modifier is still logically down. Presses do
/// the reverse. Duplicate presses and releases of unheld keys produce nothing.
fn qmp_key_diff(before: &BTreeSet<u8>, after: &BTreeSet<u8>) -> Vec<QmpInputEvent> {
    let mut events = Vec::new();

    for usage in before.difference(after) {
        if !is_modifier_usage(*usage) {
            push_key(&mut events, *usage, false);
        }
    }
    for usage in before.difference(after) {
        if is_modifier_usage(*usage) {
            push_key(&mut events, *usage, false);
        }
    }

    for usage in after.difference(before) {
        if is_modifier_usage(*usage) {
            push_key(&mut events, *usage, true);
        }
    }
    for usage in after.difference(before) {
        if !is_modifier_usage(*usage) {
            push_key(&mut events, *usage, true);
        }
    }

    events
}

fn push_key(events: &mut Vec<QmpInputEvent>, usage: u8, down: bool) {
    if let Some(qcode) = hid_to_qcode(usage) {
        events.push(QmpInputEvent::key(qcode, down));
    }
}

fn is_modifier_usage(usage: u8) -> bool {
    (MODIFIER_USAGE_START..=MODIFIER_USAGE_END).contains(&usage)
}

const fn pointer_button_name(button: PointerButton) -> &'static str {
    match button {
        PointerButton::Left => "left",
        PointerButton::Right => "right",
        PointerButton::Middle => "middle",
        PointerButton::Back => BACK_BUTTON,
        PointerButton::Forward => FORWARD_BUTTON,
    }
}

async fn run_input_task(client: QmpClient, mut receiver: UnboundedReceiver<Vec<QmpInputEvent>>) {
    while let Some(events) = receiver.recv().await {
        if events.is_empty() {
            continue;
        }

        match client.send_input_events(&events).await {
            Ok(()) => debug!(count = events.len(), "QMP input events sent"),
            Err(error) => {
                warn!(%error, "QMP input send failed; attempting one reconnect");
                if let Err(reconnect_error) = client.reconnect(1).await {
                    warn!(%reconnect_error, "QMP input reconnect failed");
                }
            }
        }
    }

    debug!("QMP input channel closed; background task exiting");
}

/// Drain input batches through the node's shared control client.
///
/// The shared client makes at most one lazy reconnect of the same connection;
/// this task must not call `disconnect`, because power or capture still own it.
async fn run_shared_input_task(
    shared: SharedQmpClient,
    mut receiver: UnboundedReceiver<Vec<QmpInputEvent>>,
) {
    while let Some(events) = receiver.recv().await {
        if events.is_empty() {
            continue;
        }

        match shared.send_input_events(&events).await {
            Ok(()) => debug!(
                count = events.len(),
                "QMP input events sent on shared client"
            ),
            Err(error) => {
                warn!(%error, "QMP input send on shared client failed; the next event retries");
            }
        }
    }

    debug!("QMP input channel closed; shared background task exiting");
}

/// Drain input batches through a dedicated input socket only.
///
/// On failure this reconnects the input half; it never connects the control
/// half, so a provisioned dual-socket VM keeps input isolated.
async fn run_dedicated_input_task(
    client: QmpClient,
    mut receiver: UnboundedReceiver<Vec<QmpInputEvent>>,
) {
    while let Some(events) = receiver.recv().await {
        if events.is_empty() {
            continue;
        }

        match client.send_input_events(&events).await {
            Ok(()) => debug!(
                count = events.len(),
                "QMP input events sent on dedicated socket"
            ),
            Err(error) => {
                warn!(%error, "QMP dedicated input send failed; reconnecting the input socket");
                if let Err(reconnect_error) = client.connect_input().await {
                    warn!(%reconnect_error, "QMP dedicated input reconnect failed");
                }
            }
        }
    }

    debug!("QMP input channel closed; dedicated background task exiting");
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use ozma_protocol::input::TouchPhase;
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tokio::sync::mpsc::error::TryRecvError;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::codes;

    fn key(code: u32, down: bool) -> InputEvent {
        InputEvent::Key {
            code,
            down,
            modifiers: KeyModifiers::default(),
        }
    }

    fn key_with(code: u32, down: bool, modifiers: u8) -> InputEvent {
        InputEvent::Key {
            code,
            down,
            modifiers: KeyModifiers::new(modifiers),
        }
    }

    fn next_batch(receiver: &mut UnboundedReceiver<Vec<QmpInputEvent>>) -> Vec<QmpInputEvent> {
        receiver.try_recv().expect("expected a queued QMP batch")
    }

    fn assert_no_batch(receiver: &mut UnboundedReceiver<Vec<QmpInputEvent>>) {
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn key_press_release_and_duplicates() {
        let (sink, mut receiver) = QmpSink::channel();

        let press = sink.submit_report(&key(0x04, true)).unwrap();
        assert_eq!(press.reports_written, 1);
        assert!(press.state_changed);
        assert_eq!(
            next_batch(&mut receiver),
            vec![QmpInputEvent::key("a", true)]
        );
        assert!(sink.held().has_key(0x04));

        let duplicate = sink.submit_report(&key(0x04, true)).unwrap();
        assert_eq!(duplicate, SubmitReport::noop());
        assert_no_batch(&mut receiver);

        let release = sink.submit_report(&key(0x04, false)).unwrap();
        assert_eq!(release.reports_written, 1);
        assert_eq!(
            next_batch(&mut receiver),
            vec![QmpInputEvent::key("a", false)]
        );
        assert!(sink.held().is_empty());

        let repeat_release = sink.submit_report(&key(0x04, false)).unwrap();
        assert_eq!(repeat_release, SubmitReport::noop());
        assert_no_batch(&mut receiver);
    }

    #[test]
    fn client_and_explicit_modifiers_are_diffed() {
        let (sink, mut receiver) = QmpSink::channel();

        sink.submit(&key_with(
            0x04,
            true,
            KeyModifiers::LEFT_CTRL | KeyModifiers::LEFT_SHIFT,
        ))
        .unwrap();
        assert_eq!(
            next_batch(&mut receiver),
            vec![
                QmpInputEvent::key("ctrl", true),
                QmpInputEvent::key("shift", true),
                QmpInputEvent::key("a", true),
            ]
        );
        assert_eq!(
            sink.held().modifiers,
            KeyModifiers::LEFT_CTRL | KeyModifiers::LEFT_SHIFT
        );

        sink.submit(&key(0x04, false)).unwrap();
        assert_eq!(
            next_batch(&mut receiver),
            vec![
                QmpInputEvent::key("a", false),
                QmpInputEvent::key("ctrl", false),
                QmpInputEvent::key("shift", false),
            ]
        );
        assert!(sink.held().is_empty());

        // Explicit modifier usage ids share the same table and state.
        sink.submit(&key(0xE0, true)).unwrap();
        assert_eq!(
            next_batch(&mut receiver),
            vec![QmpInputEvent::key("ctrl", true)]
        );
        assert_eq!(sink.held().modifiers, KeyModifiers::LEFT_CTRL);

        assert_eq!(
            sink.submit_report(&key(0xE0, true)).unwrap(),
            SubmitReport::noop()
        );
        assert_no_batch(&mut receiver);

        sink.submit(&key(0xE5, true)).unwrap();
        assert_eq!(
            next_batch(&mut receiver),
            vec![QmpInputEvent::key("shift_r", true)]
        );
        assert_eq!(
            sink.held().modifiers,
            KeyModifiers::LEFT_CTRL | KeyModifiers::RIGHT_SHIFT
        );

        sink.release_all().unwrap();
        assert_eq!(
            next_batch(&mut receiver),
            vec![
                QmpInputEvent::key("ctrl", false),
                QmpInputEvent::key("shift_r", false),
            ]
        );
        assert!(sink.held().is_empty());
    }

    #[test]
    fn pointer_button_and_wheel_translation() {
        let (sink, mut receiver) = QmpSink::channel();

        sink.submit(&InputEvent::PointerAbsolute {
            display_id: None,
            x: -5,
            y: 40_000,
        })
        .unwrap();
        assert_eq!(
            next_batch(&mut receiver),
            vec![QmpInputEvent::abs("x", 0), QmpInputEvent::abs("y", 32_767)]
        );
        assert_eq!(sink.held().pointer, Some(AbsolutePoint { x: 0, y: 32_767 }));

        sink.submit(&InputEvent::PointerRelative { dx: 5, dy: 0 })
            .unwrap();
        assert_eq!(next_batch(&mut receiver), vec![QmpInputEvent::rel("x", 5)]);

        sink.submit(&InputEvent::PointerRelative { dx: 0, dy: -2 })
            .unwrap();
        assert_eq!(next_batch(&mut receiver), vec![QmpInputEvent::rel("y", -2)]);

        sink.submit(&InputEvent::Button {
            button: PointerButton::Back,
            down: true,
        })
        .unwrap();
        assert_eq!(
            next_batch(&mut receiver),
            vec![QmpInputEvent::button("side", true)]
        );
        assert!(sink.held().has_button(PointerButton::Back));

        sink.submit(&InputEvent::Button {
            button: PointerButton::Forward,
            down: true,
        })
        .unwrap();
        assert_eq!(
            next_batch(&mut receiver),
            vec![QmpInputEvent::button("extra", true)]
        );
        assert!(sink.held().has_button(PointerButton::Forward));

        sink.submit(&InputEvent::Wheel { dx: 0, dy: 2 }).unwrap();
        assert_eq!(
            next_batch(&mut receiver),
            vec![
                QmpInputEvent::button("wheel-up", true),
                QmpInputEvent::button("wheel-up", false),
                QmpInputEvent::button("wheel-up", true),
                QmpInputEvent::button("wheel-up", false),
            ]
        );

        sink.submit(&InputEvent::Wheel { dx: 0, dy: -1 }).unwrap();
        assert_eq!(
            next_batch(&mut receiver),
            vec![
                QmpInputEvent::button("wheel-down", true),
                QmpInputEvent::button("wheel-down", false),
            ]
        );

        let error = sink
            .submit(&InputEvent::Wheel { dx: 1, dy: 0 })
            .unwrap_err();
        assert_eq!(error.code, codes::UNSUPPORTED);
        assert_eq!(error.details["event"], "wheel");
    }

    #[test]
    fn unknown_usage_is_rejected_without_changing_state() {
        let (sink, mut receiver) = QmpSink::channel();

        let error = sink.submit(&key(0x01, true)).unwrap_err();
        assert_eq!(error.code, codes::UNSUPPORTED);
        assert_eq!(error.details["hid_usage"], 0x01);
        assert!(error.remediation.is_some());
        assert!(sink.held().is_empty());
        assert_no_batch(&mut receiver);

        let invalid = sink.submit(&key(0, true)).unwrap_err();
        assert_eq!(invalid.code, codes::INVALID_REQUEST);
    }

    #[test]
    fn touch_pen_and_clipboard_are_rejected() {
        let (sink, mut receiver) = QmpSink::channel();

        let events = [
            InputEvent::Touch {
                contact: 1,
                phase: TouchPhase::Down,
                display_id: None,
                x: 10,
                y: 20,
                pressure: 1,
            },
            InputEvent::Pen {
                phase: TouchPhase::Move,
                display_id: None,
                x: 10,
                y: 20,
                pressure: 1,
                tilt_x: 0,
                tilt_y: 0,
            },
            InputEvent::Clipboard {
                text: "not on the input channel".into(),
            },
        ];

        for event in events {
            let error = sink.submit(&event).unwrap_err();
            assert_eq!(error.code, codes::UNSUPPORTED);
        }

        assert!(sink.held().is_empty());
        assert_no_batch(&mut receiver);
    }

    #[test]
    fn release_all_is_idempotent_and_clears_everything() {
        let (sink, mut receiver) = QmpSink::channel();

        sink.submit(&key(0x04, true)).unwrap();
        sink.submit(&key(0xE0, true)).unwrap();
        sink.submit(&InputEvent::Button {
            button: PointerButton::Left,
            down: true,
        })
        .unwrap();
        sink.submit(&InputEvent::Button {
            button: PointerButton::Back,
            down: true,
        })
        .unwrap();
        receiver.try_recv().unwrap();
        receiver.try_recv().unwrap();
        receiver.try_recv().unwrap();
        receiver.try_recv().unwrap();

        sink.release_all().unwrap();
        assert_eq!(
            next_batch(&mut receiver),
            vec![
                QmpInputEvent::key("a", false),
                QmpInputEvent::key("ctrl", false),
                QmpInputEvent::button("left", false),
                QmpInputEvent::button("side", false),
            ]
        );
        assert!(sink.held().is_empty());

        sink.release_all().unwrap();
        assert_no_batch(&mut receiver);

        sink.submit(&key(0x05, true)).unwrap();
        receiver.try_recv().unwrap();
        let report = sink.submit_report(&InputEvent::ReleaseAll).unwrap();
        assert_eq!(report.reports_written, 1);
        assert!(report.state_changed);
        assert_eq!(
            next_batch(&mut receiver),
            vec![QmpInputEvent::key("b", false)]
        );
        assert!(sink.held().is_empty());
    }

    #[test]
    fn closed_channel_is_a_typed_error_and_release_all_still_clears() {
        let (sink, receiver) = QmpSink::channel();
        sink.submit(&key(0x04, true)).unwrap();
        drop(receiver);

        let error = sink.submit(&key(0x05, true)).unwrap_err();
        assert_eq!(error.code, codes::INPUT_UNAVAILABLE);
        assert!(sink.held().has_key(0x04));
        assert!(!sink.held().has_key(0x05));

        // The final key-up cannot be delivered, but local state must still be
        // neutral so a reconnect does not believe "a" is still down.
        sink.release_all().unwrap_err();
        assert!(sink.held().is_empty());
    }

    fn fake_qmp_server(path: &Path) -> tokio::task::JoinHandle<Vec<Value>> {
        let listener = UnixListener::bind(path).expect("bind fake QMP socket");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept QMP client");
            let (reader, mut writer) = stream.into_split();
            writer
                .write_all(b"{\"QMP\":{\"capabilities\":[]}}\n")
                .await
                .expect("write greeting");

            let mut lines = BufReader::new(reader).lines();
            let mut received = Vec::new();
            while let Some(line) = lines.next_line().await.expect("read QMP command") {
                let value: Value = serde_json::from_str(&line).expect("parse QMP command");
                match value.get("execute").and_then(Value::as_str) {
                    Some("qmp_capabilities") => {
                        writer
                            .write_all(b"{\"return\":{}}\n")
                            .await
                            .expect("write capabilities response");
                    }
                    Some("input-send-event") => {
                        received.push(value);
                        // Fire-and-forget mode may close the socket before
                        // QEMU's reply is written; that is not a server error.
                        if writer.write_all(b"{\"return\":{}}\n").await.is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            received
        })
    }

    #[tokio::test]
    async fn spawn_sends_key_event_to_a_fake_qmp_server() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("qmp.sock");
        let server = fake_qmp_server(&socket);

        let (sink, handle) = QmpSink::spawn(QmpConfig::new(&socket)).await.unwrap();
        sink.submit(&key(0x04, true)).unwrap();

        // Closing the sink lets the background task drain the queued batch and
        // exit, which closes the QMP connection so the fake server can finish.
        drop(sink);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("QMP task should exit after the channel closes")
            .unwrap();

        let received = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("fake QMP server should exit")
            .unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0]["execute"], "input-send-event");
        assert_eq!(
            received[0]["arguments"]["events"],
            json!([{
                "type": "key",
                "data": {
                    "down": true,
                    "key": {"type": "qcode", "data": "a"}
                }
            }])
        );
    }

    #[tokio::test]
    async fn spawn_with_shared_sends_over_the_supplied_connection() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("qmp.sock");
        let server = fake_qmp_server(&socket);

        let shared = SharedQmpClient::connect(QmpConfig::new(&socket))
            .await
            .unwrap();
        let (sink, handle) = QmpSink::spawn_with_shared(shared.clone());
        sink.submit(&key(0x04, true)).unwrap();

        drop(sink);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("shared QMP task should exit after the channel closes")
            .unwrap();
        assert!(shared.is_connected().await);

        drop(shared);
        let received = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("fake QMP server should exit")
            .unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0]["execute"], "input-send-event");
        assert_eq!(
            received[0]["arguments"]["events"],
            json!([{
                "type": "key",
                "data": {
                    "down": true,
                    "key": {"type": "qcode", "data": "a"}
                }
            }])
        );
    }

    #[tokio::test]
    async fn dedicated_input_never_connects_the_control_socket() {
        let directory = tempfile::tempdir().unwrap();
        let control_path = directory.path().join("control.sock");
        let input_path = directory.path().join("input.sock");

        // A listener for the control socket that records (and rejects) any
        // connection. A dedicated input sink must never reach it.
        let control_listener = UnixListener::bind(&control_path).unwrap();
        let control_connections = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&control_connections);
        let control_task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = control_listener.accept().await else {
                    break;
                };
                observed.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });

        let input_server = fake_qmp_server(&input_path);
        let (sink, handle) =
            QmpSink::spawn_dedicated_input(QmpConfig::dual(&control_path, &input_path))
                .await
                .unwrap();
        assert_eq!(control_connections.load(Ordering::SeqCst), 0);

        sink.submit(&key(0x04, true)).unwrap();
        drop(sink);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("dedicated QMP task should exit after the channel closes")
            .unwrap();

        let received = tokio::time::timeout(Duration::from_secs(5), input_server)
            .await
            .expect("fake input QMP server should exit")
            .unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0]["execute"], "input-send-event");
        assert_eq!(control_connections.load(Ordering::SeqCst), 0);
        control_task.abort();
    }
}
