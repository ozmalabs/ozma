// SPDX-License-Identifier: AGPL-3.0-only
//! Bounded media-export hotplug events for the node `/usb/hotplug` view.
//!
//! Only events actually caused by a route or observed from ConfigFS are
//! recorded. The log never synthesises a kernel attach/detach event, never
//! includes a backing path, and drops the oldest entry when it reaches its
//! capacity.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// Maximum retained media hotplug events.
pub const MAX_USB_MEDIA_HOTPLUG_EVENTS: usize = 64;

/// A media export or attachment transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsbMediaHotplugKind {
    /// A userspace SCSI export started (configured, not necessarily running).
    ExportStart,
    /// A userspace SCSI export stopped.
    ExportStop,
    /// Media was attached to a mass-storage function.
    Attach,
    /// Media was detached from a mass-storage function.
    Detach,
    /// The gadget was observed transitioning from bound to unbound.
    Unbind,
}

impl UsbMediaHotplugKind {
    /// Stable lower-case event name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExportStart => "export_start",
            Self::ExportStop => "export_stop",
            Self::Attach => "attach",
            Self::Detach => "detach",
            Self::Unbind => "unbind",
        }
    }
}

/// One bounded media hotplug event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbMediaHotplugEvent {
    /// Monotonic sequence number within one log.
    pub sequence: u64,
    /// Transition kind.
    pub kind: UsbMediaHotplugKind,
    /// Mass-storage function/instance, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    /// Resulting state (`configured`, `running`, `stopped`, `attached`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// A fixed-capacity, drop-oldest media hotplug log.
#[derive(Clone, Debug)]
pub struct UsbMediaHotplugLog {
    next_sequence: u64,
    capacity: usize,
    events: VecDeque<UsbMediaHotplugEvent>,
}

impl Default for UsbMediaHotplugLog {
    fn default() -> Self {
        Self::new(MAX_USB_MEDIA_HOTPLUG_EVENTS)
    }
}

impl UsbMediaHotplugLog {
    /// Create a log with the given capacity (clamped to the supported range).
    pub fn new(capacity: usize) -> Self {
        Self {
            next_sequence: 1,
            capacity: capacity.clamp(1, MAX_USB_MEDIA_HOTPLUG_EVENTS),
            events: VecDeque::new(),
        }
    }

    /// Retained-event capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of retained events.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether no events are retained.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Sequence number the next recorded event will receive.
    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// Record a transition, returning the stored event.
    pub fn record(
        &mut self,
        kind: UsbMediaHotplugKind,
        function: Option<String>,
        state: Option<String>,
    ) -> UsbMediaHotplugEvent {
        let event = UsbMediaHotplugEvent {
            sequence: self.next_sequence,
            kind,
            function,
            state,
        };
        self.next_sequence = self.next_sequence.saturating_add(1);
        if self.events.len() == self.capacity {
            self.events.pop_front();
        }
        self.events.push_back(event.clone());
        event
    }

    /// Retained events, oldest first.
    pub fn events(&self) -> Vec<UsbMediaHotplugEvent> {
        self.events.iter().cloned().collect()
    }

    /// Events with a sequence strictly greater than `since`, newest `limit`
    /// retained (oldest first within the returned window).
    pub fn events_since(&self, since: u64, limit: usize) -> Vec<UsbMediaHotplugEvent> {
        let limit = limit.clamp(1, self.capacity);
        let mut events: Vec<UsbMediaHotplugEvent> = self
            .events
            .iter()
            .filter(|event| event.sequence > since)
            .cloned()
            .collect();
        if events.len() > limit {
            events = events.split_off(events.len() - limit);
        }
        events
    }

    /// The most recent event.
    pub fn latest(&self) -> Option<&UsbMediaHotplugEvent> {
        self.events.back()
    }

    /// Remove and return all retained events.
    pub fn drain(&mut self) -> Vec<UsbMediaHotplugEvent> {
        self.events.drain(..).collect()
    }
}
