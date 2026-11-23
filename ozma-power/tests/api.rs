// SPDX-License-Identifier: AGPL-3.0-only
//! Public value types, serde, parsing, and trait object safety.

use std::str::FromStr;

use async_trait::async_trait;
use ozma_power::{PowerAction, PowerBackend, PowerError, PowerState, PowerStatus};
use serde_json::json;

#[test]
fn stable_state_names_match_python() {
    let states = [
        (PowerState::running(), "running"),
        (PowerState::blocked(), "blocked"),
        (PowerState::paused(), "paused"),
        (PowerState::shutdown(), "shutdown"),
        (PowerState::shutoff(), "shutoff"),
        (PowerState::crashed(), "crashed"),
        (PowerState::pmsuspended(), "pmsuspended"),
        (PowerState::unknown(), "unknown"),
    ];

    for (state, expected) in states {
        assert_eq!(state.status, expected);
        assert_eq!(state.to_string(), expected);
        assert_eq!(state.kind().as_str(), expected);
        assert_eq!(state.is_running(), expected == "running");
    }
}

#[test]
fn state_new_trims_lowercases_and_defaults_to_unknown() {
    assert_eq!(PowerState::new("  RUNNING\n").status, "running");
    assert_eq!(PowerState::new("").status, "unknown");
    assert_eq!(PowerState::new("   ").status, "unknown");
}

#[test]
fn libvirt_aliases_are_normalized() {
    for (raw, expected) in [
        ("running", "running"),
        ("blocked", "blocked"),
        ("paused", "paused"),
        ("in shutdown", "shutdown"),
        ("shut off", "shutoff"),
        ("shutoff", "shutoff"),
        ("crashed", "crashed"),
        ("pmsuspended", "pmsuspended"),
        ("idle", "unknown"),
        ("", "unknown"),
    ] {
        assert_eq!(
            PowerState::from_libvirt(raw).status,
            expected,
            "raw libvirt state {raw:?}"
        );
    }
}

#[test]
fn qmp_states_are_normalized() {
    for (raw, expected) in [
        ("running", "running"),
        ("paused", "paused"),
        ("shutdown", "shutdown"),
        ("suspended", "pmsuspended"),
        ("blocked", "blocked"),
        ("prelaunch", "unknown"),
        ("io-error", "unknown"),
        ("guest-panicked", "unknown"),
    ] {
        assert_eq!(
            PowerState::from_qmp(raw).status,
            expected,
            "raw QMP state {raw:?}"
        );
    }
}

#[test]
fn state_serde_shape_matches_python_query_status() {
    assert_eq!(
        serde_json::to_value(PowerState::running()).unwrap(),
        json!({"status": "running"})
    );

    let decoded: PowerState = serde_json::from_value(json!({"status": "paused"})).unwrap();
    assert_eq!(decoded, PowerState::paused());

    for state in [
        PowerState::running(),
        PowerState::blocked(),
        PowerState::paused(),
        PowerState::shutdown(),
        PowerState::shutoff(),
        PowerState::crashed(),
        PowerState::pmsuspended(),
        PowerState::unknown(),
    ] {
        let encoded = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<PowerState>(&encoded).unwrap(), state);
    }
}

#[test]
fn state_parsing_round_trips() {
    for name in [
        "running",
        "blocked",
        "paused",
        "shutdown",
        "shutoff",
        "crashed",
        "pmsuspended",
        "unknown",
    ] {
        let state = PowerState::from_str(name).unwrap();
        assert_eq!(state.status, name);
        assert_eq!(PowerStatus::from_str(name).unwrap().as_str(), name);
    }

    assert!(matches!(
        PowerState::from_str("teleporting").unwrap_err(),
        PowerError::InvalidState(_)
    ));
    assert!(matches!(
        PowerStatus::from_str("teleporting").unwrap_err(),
        PowerError::InvalidState(_)
    ));

    // Aliases classify but are not emitted.
    assert_eq!(PowerState::from_str("shut off").unwrap().status, "shutoff");
    assert_eq!(
        PowerStatus::from_str("in shutdown").unwrap(),
        PowerStatus::Shutdown
    );
    assert_eq!(
        PowerStatus::from_str("suspended").unwrap(),
        PowerStatus::Pmsuspended
    );
}

#[test]
fn action_names_match_the_python_http_surface() {
    let expected = [
        (PowerAction::Start, "on"),
        (PowerAction::Shutdown, "off"),
        (PowerAction::Reset, "reset"),
        (PowerAction::Stop, "force-off"),
    ];

    for (action, name) in expected {
        assert_eq!(action.as_str(), name);
        assert_eq!(action.to_string(), name);
        assert_eq!(serde_json::to_value(action).unwrap(), json!(name));

        let decoded: PowerAction = serde_json::from_value(json!(name)).unwrap();
        assert_eq!(decoded, action);
    }

    assert_eq!(PowerAction::ALL.len(), 4);
}

#[test]
fn action_parsing_accepts_canonical_names_and_aliases() {
    let aliases = [
        ("on", PowerAction::Start),
        ("start", PowerAction::Start),
        ("cont", PowerAction::Start),
        ("resume", PowerAction::Start),
        ("off", PowerAction::Shutdown),
        ("shutdown", PowerAction::Shutdown),
        ("powerdown", PowerAction::Shutdown),
        ("power-off", PowerAction::Shutdown),
        ("reset", PowerAction::Reset),
        ("force-off", PowerAction::Stop),
        ("force_off", PowerAction::Stop),
        ("stop", PowerAction::Stop),
        ("destroy", PowerAction::Stop),
        (" ON ", PowerAction::Start),
    ];

    for (name, expected) in aliases {
        assert_eq!(PowerAction::from_str(name).unwrap(), expected, "{name:?}");
    }

    for action in PowerAction::ALL {
        assert_eq!(PowerAction::from_str(action.as_str()).unwrap(), action);
    }

    let error = PowerAction::from_str("explode").unwrap_err();
    assert!(matches!(error, PowerError::InvalidState(_)));
    assert!(error.to_string().contains("on, off, reset, force-off"));
}

struct DummyBackend {
    state: PowerState,
}

#[async_trait]
impl PowerBackend for DummyBackend {
    async fn query_status(&mut self) -> Result<PowerState, PowerError> {
        Ok(self.state.clone())
    }

    async fn cont(&mut self) -> Result<(), PowerError> {
        self.state = PowerState::running();
        Ok(())
    }

    async fn system_powerdown(&mut self) -> Result<(), PowerError> {
        Ok(())
    }

    async fn system_reset(&mut self) -> Result<(), PowerError> {
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), PowerError> {
        Ok(())
    }
}

#[tokio::test]
async fn power_backend_is_object_safe() {
    let mut backend: Box<dyn PowerBackend> = Box::new(DummyBackend {
        state: PowerState::paused(),
    });

    assert_eq!(backend.query_status().await.unwrap(), PowerState::paused());
    backend.cont().await.unwrap();
    assert_eq!(backend.query_status().await.unwrap(), PowerState::running());
    backend.system_powerdown().await.unwrap();
    backend.system_reset().await.unwrap();
    backend.stop().await.unwrap();
}
