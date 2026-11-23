// SPDX-License-Identifier: AGPL-3.0-only
//! A deterministic in-memory [`VirtualDisplayProvider`].
//!
//! The fake is the CI contract for everything above the platform layer: API,
//! session, input routing, and UI can be built and tested without a kernel
//! module. It is thread-safe and uses a stable, id-sorted layout so snapshots
//! are reproducible.

use std::sync::{Arc, Mutex, MutexGuard};

use ozma_protocol::display::{
    DisplayEvent, DisplayId, DisplayMode, DisplayPlacement, DisplayTopology, DpmsState,
    VirtualDisplayCaps, VirtualDisplaySpec,
};
use ozma_protocol::Result;
use tokio::sync::broadcast;

use crate::error;
use crate::mapping::logical_size;
use crate::provider::{
    DisplayEventStream, DisplayPrivacy, DisplayState, VirtualDisplay, VirtualDisplayProvider,
};

const EVENT_CAPACITY: usize = 128;

#[derive(Clone, Debug)]
struct Entry {
    spec: VirtualDisplaySpec,
    mode: DisplayMode,
    state: DisplayState,
    privacy: DisplayPrivacy,
}

impl Entry {
    fn handle(&self) -> VirtualDisplay {
        VirtualDisplay::new(self.spec.clone(), self.mode.clone(), self.state)
    }
}

#[derive(Default)]
struct FakeState {
    displays: Vec<Entry>,
    /// `None` means "derive a left-to-right layout from the current displays".
    explicit_topology: Option<DisplayTopology>,
}

/// Thread-safe fake provider. Clone the handle: clones share one state.
#[derive(Clone)]
pub struct FakeVirtualDisplayProvider {
    caps: VirtualDisplayCaps,
    state: Arc<Mutex<FakeState>>,
    events: broadcast::Sender<DisplayEvent>,
}

impl Default for FakeVirtualDisplayProvider {
    fn default() -> Self {
        Self::new(VirtualDisplayCaps::fake())
    }
}

impl FakeVirtualDisplayProvider {
    /// Build a provider with explicit capabilities. Use
    /// `VirtualDisplayCaps::none()` to exercise the unsupported path.
    pub fn new(caps: VirtualDisplayCaps) -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            caps,
            state: Arc::new(Mutex::new(FakeState::default())),
            events,
        }
    }

    /// The provider's capabilities, without consuming the trait object.
    pub fn caps(&self) -> &VirtualDisplayCaps {
        &self.caps
    }

    /// Snapshot of every attached display, in creation order.
    pub fn displays(&self) -> Vec<VirtualDisplay> {
        self.lock().displays.iter().map(Entry::handle).collect()
    }

    /// Snapshot one display.
    pub fn get(&self, id: &DisplayId) -> Option<VirtualDisplay> {
        self.lock()
            .displays
            .iter()
            .find(|entry| &entry.spec.id == id)
            .map(Entry::handle)
    }

    /// The privacy policy currently attached to a display.
    pub fn privacy(&self, id: &DisplayId) -> Option<DisplayPrivacy> {
        self.lock()
            .displays
            .iter()
            .find(|entry| &entry.spec.id == id)
            .map(|entry| entry.privacy.clone())
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        // A poisoned lock only means another test thread panicked while
        // mutating the fake; the invariant-protected state is still readable.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn emit(&self, event: DisplayEvent) {
        // No subscribers is normal (nobody is watching), not an error.
        let _ = self.events.send(event);
    }

    fn derived_topology(state: &FakeState) -> DisplayTopology {
        let mut x = 0i32;
        let mut displays = Vec::with_capacity(state.displays.len());
        for (index, entry) in state.displays.iter().enumerate() {
            let (width, height) =
                logical_size(&entry.mode, entry.spec.rotation, entry.spec.scale_percent);
            displays.push(DisplayPlacement {
                display_id: entry.spec.id.clone(),
                x,
                y: 0,
                width,
                height,
                scale_percent: entry.spec.scale_percent,
                rotation: entry.spec.rotation,
                primary: index == 0,
            });
            x = x.saturating_add(width as i32);
        }
        DisplayTopology::new(displays)
    }

    fn topology_locked(&self, state: &FakeState) -> DisplayTopology {
        state
            .explicit_topology
            .clone()
            .unwrap_or_else(|| Self::derived_topology(state))
    }

    fn emit_topology(&self, state: &FakeState) {
        self.emit(DisplayEvent::TopologyChanged {
            topology: self.topology_locked(state),
        });
    }

    fn validate_topology(state: &FakeState, topology: &DisplayTopology) -> Result<()> {
        if topology.displays.len() != state.displays.len() {
            return Err(error::invalid_topology(format!(
                "Expected {} placements, received {}",
                state.displays.len(),
                topology.displays.len()
            )));
        }
        let mut seen = Vec::with_capacity(topology.displays.len());
        for placement in &topology.displays {
            if placement.width == 0 || placement.height == 0 {
                return Err(error::invalid_topology(format!(
                    "Display {} has a zero-sized placement",
                    placement.display_id
                )));
            }
            if seen.contains(&placement.display_id) {
                return Err(error::invalid_topology(format!(
                    "Display {} appears twice",
                    placement.display_id
                )));
            }
            if !state
                .displays
                .iter()
                .any(|entry| entry.spec.id == placement.display_id)
            {
                return Err(error::unknown_display(&placement.display_id));
            }
            seen.push(placement.display_id.clone());
        }
        Ok(())
    }
}

impl VirtualDisplayProvider for FakeVirtualDisplayProvider {
    fn capabilities(&self) -> VirtualDisplayCaps {
        self.caps.clone()
    }

    fn create(&self, spec: VirtualDisplaySpec) -> Result<VirtualDisplay> {
        if !self.caps.supported {
            return Err(error::unsupported(
                "The selected virtual-display backend is not supported on this platform",
            )
            .with_remediation(
                "Install a supported display backend or use the labelled mirror fallback",
            ));
        }
        spec.validate()?;

        let mode = spec.preferred.clone();
        let id = spec.id.clone();
        let mut state = self.lock();

        if state.displays.iter().any(|entry| entry.spec.id == id) {
            return Err(error::create_failed(format!("Display {id} already exists"))
                .with_details(serde_json::json!({ "display_id": id.as_str() }))
                .with_remediation(
                    "Destroy the old display or reconnect using its existing identity",
                ));
        }
        if state.displays.len() >= usize::from(self.caps.max_outputs) {
            return Err(error::create_failed(format!(
                "The provider already has {} of {} outputs",
                state.displays.len(),
                self.caps.max_outputs
            ))
            .with_details(serde_json::json!({
                "limit": self.caps.max_outputs,
                "attached": state.displays.len(),
            }))
            .with_remediation("Detach another remote display before attaching a new one"));
        }

        let (width, height) = logical_size(&mode, spec.rotation, spec.scale_percent);

        state.displays.push(Entry {
            spec: spec.clone(),
            mode: mode.clone(),
            state: DisplayState::Online,
            privacy: DisplayPrivacy::default(),
        });

        if let Some(topology) = state.explicit_topology.as_mut() {
            let x = topology.bounds().map(|(_, _, max_x, _)| max_x).unwrap_or(0);
            let primary = topology.displays.is_empty();
            topology.displays.push(DisplayPlacement {
                display_id: id.clone(),
                x,
                y: 0,
                width,
                height,
                scale_percent: spec.scale_percent,
                rotation: spec.rotation,
                primary,
            });
        }

        let handle = state.displays.last().expect("just pushed").handle();
        self.emit(DisplayEvent::Added {
            display: spec.clone(),
        });
        self.emit_topology(&state);
        Ok(handle)
    }

    fn destroy(&self, id: DisplayId) -> Result<()> {
        let mut state = self.lock();
        let before = state.displays.len();
        state.displays.retain(|entry| entry.spec.id != id);
        if state.displays.len() == before {
            // Idempotent: the desired state (absent) already holds.
            return Ok(());
        }

        if let Some(topology) = state.explicit_topology.as_mut() {
            topology.displays.retain(|p| p.display_id != id);
        }
        if state
            .explicit_topology
            .as_ref()
            .is_some_and(|topology| topology.displays.is_empty())
        {
            state.explicit_topology = None;
        }

        self.emit(DisplayEvent::Removed {
            display_id: id.clone(),
        });
        self.emit_topology(&state);
        Ok(())
    }

    fn set_mode(&self, id: DisplayId, mode: DisplayMode) -> Result<()> {
        let mut state = self.lock();
        let index = state
            .displays
            .iter()
            .position(|entry| entry.spec.id == id)
            .ok_or_else(|| error::unknown_display(&id))?;

        let entry = &state.displays[index];
        if !entry.spec.modes.contains(&mode) {
            return Err(error::mode_rejected(
                &id,
                &mode,
                "the mode is not advertised in the display EDID",
            ));
        }

        state.displays[index].mode = mode.clone();

        let entry = &state.displays[index];
        let (width, height) = logical_size(&mode, entry.spec.rotation, entry.spec.scale_percent);

        if let Some(topology) = state.explicit_topology.as_mut() {
            if let Some(placement) = topology.displays.iter_mut().find(|p| p.display_id == id) {
                placement.width = width;
                placement.height = height;
            }
        }

        self.emit(DisplayEvent::ModeChanged {
            display_id: id,
            mode,
        });
        self.emit_topology(&state);
        Ok(())
    }

    fn set_dpms(&self, id: DisplayId, dpms: DpmsState) -> Result<()> {
        let mut state = self.lock();
        let entry = state
            .displays
            .iter_mut()
            .find(|entry| entry.spec.id == id)
            .ok_or_else(|| error::unknown_display(&id))?;
        entry.state = DisplayState::from_dpms(dpms);
        self.emit(DisplayEvent::DpmsChanged {
            display_id: id,
            state: dpms,
        });
        Ok(())
    }

    fn set_privacy(&self, id: DisplayId, policy: &DisplayPrivacy) -> Result<()> {
        let mut state = self.lock();
        let entry = state
            .displays
            .iter_mut()
            .find(|entry| entry.spec.id == id)
            .ok_or_else(|| error::unknown_display(&id))?;
        entry.privacy = policy.clone();
        Ok(())
    }

    fn set_topology(&self, topology: DisplayTopology) -> Result<()> {
        let mut state = self.lock();
        Self::validate_topology(&state, &topology)?;
        state.explicit_topology = Some(topology.clone());
        self.emit(DisplayEvent::TopologyChanged { topology });
        Ok(())
    }

    fn topology(&self) -> Result<DisplayTopology> {
        let state = self.lock();
        Ok(self.topology_locked(&state))
    }

    fn events(&self) -> DisplayEventStream {
        self.events.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_protocol::display::Rotation;

    fn mode() -> DisplayMode {
        DisplayMode::new(1920, 1080, 60)
    }

    fn spec(id: &str) -> VirtualDisplaySpec {
        let mut spec = VirtualDisplaySpec::new(id, format!("Remote {id}"), (300, 190), mode());
        spec.modes.push(DisplayMode::new(1280, 720, 60));
        spec
    }

    fn provider(max_outputs: u16) -> FakeVirtualDisplayProvider {
        let mut caps = VirtualDisplayCaps::fake();
        caps.max_outputs = max_outputs;
        FakeVirtualDisplayProvider::new(caps)
    }

    #[test]
    fn creates_and_destroys() {
        let provider = provider(2);
        let display = provider.create(spec("remote-1")).unwrap();
        assert_eq!(display.id(), &DisplayId::new("remote-1"));
        assert_eq!(display.mode(), &mode());
        assert_eq!(display.state(), DisplayState::Online);
        assert_eq!(provider.displays().len(), 1);

        provider.destroy(DisplayId::new("remote-1")).unwrap();
        assert!(provider.displays().is_empty());

        // Documented as idempotent.
        provider.destroy(DisplayId::new("remote-1")).unwrap();
    }

    #[test]
    fn duplicate_id_is_rejected() {
        let provider = provider(4);
        provider.create(spec("remote-1")).unwrap();
        let err = provider.create(spec("remote-1")).unwrap_err();
        assert_eq!(err.code, ozma_protocol::codes::DISPLAY_CREATE_FAILED);
        assert!(err.remediation.is_some());
        assert_eq!(
            err.details["display_id"],
            serde_json::Value::String("remote-1".into())
        );
    }

    #[test]
    fn enforces_max_outputs() {
        let provider = provider(1);
        provider.create(spec("remote-1")).unwrap();
        let err = provider.create(spec("remote-2")).unwrap_err();
        assert_eq!(err.code, ozma_protocol::codes::DISPLAY_CREATE_FAILED);
        assert_eq!(err.details["limit"], 1);
        assert_eq!(err.details["attached"], 1);
    }

    #[test]
    fn unsupported_caps_refuse_creation() {
        let provider = FakeVirtualDisplayProvider::new(VirtualDisplayCaps::none());
        let err = provider.create(spec("remote-1")).unwrap_err();
        assert_eq!(err.code, ozma_protocol::codes::DISPLAY_UNSUPPORTED);
        assert!(err.remediation.is_some());
    }

    #[test]
    fn invalid_spec_is_rejected() {
        let provider = provider(4);
        let mut bad = spec("remote-1");
        bad.name = "   ".into();
        let err = provider.create(bad).unwrap_err();
        assert_eq!(err.code, ozma_protocol::codes::INVALID_REQUEST);
    }

    #[test]
    fn changes_modes_and_rejects_unadvertised_modes() {
        let provider = provider(4);
        provider.create(spec("remote-1")).unwrap();
        provider
            .set_mode(DisplayId::new("remote-1"), DisplayMode::new(1280, 720, 60))
            .unwrap();
        assert_eq!(
            provider.get(&DisplayId::new("remote-1")).unwrap().mode(),
            &DisplayMode::new(1280, 720, 60)
        );

        let err = provider
            .set_mode(DisplayId::new("remote-1"), DisplayMode::new(3840, 2160, 60))
            .unwrap_err();
        assert_eq!(err.code, ozma_protocol::codes::DISPLAY_MODE_REJECTED);
        assert_eq!(err.details["mode"], "3840x2160@60");

        let err = provider
            .set_mode(DisplayId::new("missing"), DisplayMode::new(1280, 720, 60))
            .unwrap_err();
        assert_eq!(err.code, ozma_protocol::codes::NOT_FOUND);
    }

    #[test]
    fn changes_dpms_and_reports_state() {
        let provider = provider(4);
        provider.create(spec("remote-1")).unwrap();
        let id = DisplayId::new("remote-1");

        provider.set_dpms(id.clone(), DpmsState::Off).unwrap();
        let display = provider.get(&id).unwrap();
        assert_eq!(display.state(), DisplayState::Off);
        assert!(!display.is_active());

        provider.set_dpms(id.clone(), DpmsState::On).unwrap();
        assert_eq!(provider.get(&id).unwrap().state(), DisplayState::Online);

        let err = provider
            .set_dpms(DisplayId::new("missing"), DpmsState::Off)
            .unwrap_err();
        assert_eq!(err.code, ozma_protocol::codes::NOT_FOUND);
    }

    #[test]
    fn privacy_is_stored_per_display() {
        let provider = provider(4);
        provider.create(spec("remote-1")).unwrap();
        let id = DisplayId::new("remote-1");
        assert_eq!(provider.privacy(&id), Some(DisplayPrivacy::default()));

        let policy = DisplayPrivacy {
            indicator: false,
            local_action: crate::provider::PrivacyAction::Blank,
            blank_on_disconnect: true,
        };
        provider.set_privacy(id.clone(), &policy).unwrap();
        assert_eq!(provider.privacy(&id), Some(policy));

        let err = provider
            .set_privacy(DisplayId::new("missing"), &DisplayPrivacy::default())
            .unwrap_err();
        assert_eq!(err.code, ozma_protocol::codes::NOT_FOUND);
    }

    #[test]
    fn emits_added_removed_mode_dpms_and_topology_events() {
        let provider = provider(4);
        let mut events = provider.events();
        let id = DisplayId::new("remote-1");

        provider.create(spec("remote-1")).unwrap();
        assert!(matches!(
            events.try_recv().unwrap(),
            DisplayEvent::Added { .. }
        ));
        let topology = match events.try_recv().unwrap() {
            DisplayEvent::TopologyChanged { topology } => topology,
            other => panic!("expected topology event, got {other:?}"),
        };
        assert_eq!(topology.displays.len(), 1);
        assert_eq!(topology.displays[0].width, 1920);

        provider
            .set_mode(id.clone(), DisplayMode::new(1280, 720, 60))
            .unwrap();
        assert!(matches!(
            events.try_recv().unwrap(),
            DisplayEvent::ModeChanged { .. }
        ));
        assert!(matches!(
            events.try_recv().unwrap(),
            DisplayEvent::TopologyChanged { .. }
        ));

        provider.set_dpms(id.clone(), DpmsState::Suspend).unwrap();
        assert!(matches!(
            events.try_recv().unwrap(),
            DisplayEvent::DpmsChanged { .. }
        ));

        provider.destroy(id).unwrap();
        assert!(matches!(
            events.try_recv().unwrap(),
            DisplayEvent::Removed { .. }
        ));
        assert!(matches!(
            events.try_recv().unwrap(),
            DisplayEvent::TopologyChanged { .. }
        ));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn derives_left_to_right_topology() {
        let provider = provider(4);
        provider.create(spec("a")).unwrap();
        provider.create(spec("b")).unwrap();
        let topology = provider.topology().unwrap();
        assert_eq!(topology.displays.len(), 2);
        assert_eq!(topology.displays[0].x, 0);
        assert_eq!(topology.displays[0].primary, true);
        assert_eq!(topology.displays[1].x, 1920);
        assert_eq!(topology.displays[1].primary, false);
        assert_eq!(topology.bounds(), Some((0, 0, 3840, 1080)));
    }

    #[test]
    fn topology_accounts_for_rotation_and_scale() {
        let provider = provider(4);
        let mut spec = spec("a");
        spec.rotation = Rotation::Deg90;
        spec.scale_percent = 125;
        provider.create(spec).unwrap();
        let topology = provider.topology().unwrap();
        // 1920x1080 at 125% is 1536x864, then rotated 90° is 864x1536.
        assert_eq!(topology.displays[0].width, 864);
        assert_eq!(topology.displays[0].height, 1536);
    }

    #[test]
    fn accepts_and_validates_explicit_topology() {
        let provider = provider(4);
        provider.create(spec("a")).unwrap();
        provider.create(spec("b")).unwrap();

        let topology = DisplayTopology::new(vec![
            DisplayPlacement {
                display_id: DisplayId::new("a"),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale_percent: 100,
                rotation: Rotation::Deg0,
                primary: false,
            },
            DisplayPlacement {
                display_id: DisplayId::new("b"),
                x: 0,
                y: 1080,
                width: 1920,
                height: 1080,
                scale_percent: 100,
                rotation: Rotation::Deg0,
                primary: true,
            },
        ]);
        let mut events = provider.events();
        provider.set_topology(topology.clone()).unwrap();
        assert_eq!(
            provider.topology().unwrap(),
            topology,
            "OS-provided layout must be preserved"
        );
        assert!(matches!(
            events.try_recv().unwrap(),
            DisplayEvent::TopologyChanged { .. }
        ));

        // Unknown display in a topology is a structured not_found.
        let mut bad = topology.clone();
        bad.displays[1].display_id = DisplayId::new("missing");
        let err = provider.set_topology(bad).unwrap_err();
        assert_eq!(err.code, ozma_protocol::codes::NOT_FOUND);

        // Missing placements are rejected.
        let mut bad = topology;
        bad.displays.pop();
        let err = provider.set_topology(bad).unwrap_err();
        assert_eq!(err.code, ozma_protocol::codes::INVALID_REQUEST);

        // Zero-sized placements are rejected.
        let mut bad = DisplayTopology::new(vec![
            DisplayPlacement {
                display_id: DisplayId::new("a"),
                x: 0,
                y: 0,
                width: 0,
                height: 1080,
                scale_percent: 100,
                rotation: Rotation::Deg0,
                primary: true,
            },
            DisplayPlacement {
                display_id: DisplayId::new("b"),
                x: 0,
                y: 1080,
                width: 1920,
                height: 1080,
                scale_percent: 100,
                rotation: Rotation::Deg0,
                primary: false,
            },
        ]);
        bad.displays[0].width = 0;
        let err = provider.set_topology(bad).unwrap_err();
        assert_eq!(err.code, ozma_protocol::codes::INVALID_REQUEST);
    }

    #[test]
    fn new_display_extends_explicit_topology_to_the_right() {
        let provider = provider(4);
        provider.create(spec("a")).unwrap();
        let topology = DisplayTopology::new(vec![DisplayPlacement {
            display_id: DisplayId::new("a"),
            x: 100,
            y: 50,
            width: 1920,
            height: 1080,
            scale_percent: 100,
            rotation: Rotation::Deg0,
            primary: true,
        }]);
        provider.set_topology(topology).unwrap();
        provider.create(spec("b")).unwrap();

        let topology = provider.topology().unwrap();
        assert_eq!(topology.displays.len(), 2);
        assert_eq!(topology.displays[1].x, 2020);
        assert_eq!(topology.displays[1].y, 0);
        assert!(!topology.displays[1].primary);
    }

    #[test]
    fn event_streams_are_independent() {
        let provider = provider(4);
        let mut first = provider.events();
        let mut second = provider.events();
        provider.create(spec("a")).unwrap();

        assert!(matches!(
            first.try_recv().unwrap(),
            DisplayEvent::Added { .. }
        ));
        assert!(matches!(
            second.try_recv().unwrap(),
            DisplayEvent::Added { .. }
        ));
    }

    #[test]
    fn cloned_handles_share_state() {
        let provider = provider(4);
        let clone = provider.clone();
        provider.create(spec("a")).unwrap();
        assert_eq!(clone.displays().len(), 1);
        clone.destroy(DisplayId::new("a")).unwrap();
        assert!(provider.displays().is_empty());
    }

    #[test]
    fn provider_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<FakeVirtualDisplayProvider>();
    }
}
