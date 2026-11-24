// SPDX-License-Identifier: AGPL-3.0-only
//! USB/IP server-side pairing, TLS fail-closed policy, and bounded hotplug
//! events. All tests use a scripted runner; no real `usbip`, kernel module, or
//! socket is touched.

use std::collections::VecDeque;
use std::sync::Mutex;

use ozma_usb::{
    CommandOutput, CommandRunner, CommandSpec, SubprocessUsbIpController, UsbError,
    UsbIpAuthorization, UsbIpConfig, UsbIpController, UsbIpHotplugKind, UsbIpHotplugLog, UsbIpKey,
    UsbIpTlsPolicy, UsbIpToken, DEFAULT_USBIP_PORT, MAX_USBIP_HOTPLUG_EVENTS,
};

#[derive(Debug, Default)]
struct QueueRunner {
    calls: Mutex<Vec<CommandSpec>>,
    responses: Mutex<VecDeque<Result<CommandOutput, UsbError>>>,
}

impl QueueRunner {
    fn push_ok(&self) {
        self.responses
            .lock()
            .unwrap()
            .push_back(Ok(CommandOutput::success("")));
    }

    fn calls(&self) -> Vec<CommandSpec> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for QueueRunner {
    fn run(&self, command: &CommandSpec) -> Result<CommandOutput, UsbError> {
        self.calls.lock().unwrap().push(command.clone());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(CommandOutput::success("")))
    }
}

fn make_controller(config: UsbIpConfig) -> SubprocessUsbIpController<QueueRunner> {
    SubprocessUsbIpController::new(QueueRunner::default(), "usbip", config)
}

fn paired_config() -> UsbIpConfig {
    let authorization = UsbIpAuthorization {
        allowed_hosts: vec!["192.0.2.10".to_owned()],
        allowed_keys: vec![UsbIpKey::new("SHA256:ab:cd:ef:01:23:45:67").unwrap()],
        allowed_tokens: vec![UsbIpToken::new("ozma-peer-token-0123456789").unwrap()],
    };
    UsbIpConfig::server("127.0.0.1", DEFAULT_USBIP_PORT).with_authorization(authorization)
}

const TOKEN: &str = "ozma-peer-token-0123456789";
const FINGERPRINT: &str = "SHA256:ab:cd:ef:01:23:45:67";

#[test]
fn server_side_pairing_checks_host_token_and_fingerprint() {
    let controller = make_controller(paired_config());
    let grant = controller
        .authorize_peer("192.0.2.10", Some(TOKEN), Some(FINGERPRINT))
        .unwrap();
    assert_eq!(grant.host, "192.0.2.10");
    assert!(grant.token_presented);
    assert_eq!(grant.key.as_ref().unwrap().fingerprint, FINGERPRINT);

    let cases: Vec<(&str, Option<&str>, Option<&str>, &str)> = vec![
        ("192.0.2.11", Some(TOKEN), Some(FINGERPRINT), "host"),
        ("192.0.2.10", None, Some(FINGERPRINT), "missing token"),
        (
            "192.0.2.10",
            Some("wrong-token-0000000000"),
            Some(FINGERPRINT),
            "wrong token",
        ),
        ("192.0.2.10", Some(TOKEN), None, "missing fingerprint"),
        (
            "192.0.2.10",
            Some(TOKEN),
            Some("SHA256:00:00:00:00:00:00:00:00"),
            "wrong fingerprint",
        ),
    ];
    for (host, token, fingerprint, label) in cases {
        let error = controller
            .authorize_peer(host, token, fingerprint)
            .unwrap_err();
        assert_eq!(error.kind(), "usbip-unauthorized", "{label}");
        assert!(
            !error.to_string().contains(TOKEN),
            "{label} leaked the token"
        );
        if let Some(fingerprint) = fingerprint {
            assert!(
                !error.to_string().contains(fingerprint),
                "{label} leaked the fingerprint"
            );
        }
    }

    // The grant itself never retains the raw token.
    assert!(!format!("{grant:?}").contains(TOKEN));
    assert!(controller.runner().calls().is_empty());
}

#[test]
fn export_with_token_runs_bind_only_after_pairing() {
    let controller = make_controller(paired_config());
    controller.runner().push_ok();

    controller
        .export_with_token("1-1", "192.0.2.10", Some(TOKEN), Some(FINGERPRINT))
        .unwrap();
    assert_eq!(
        controller.runner().calls(),
        vec![CommandSpec::new("usbip", ["bind", "-b", "1-1"])]
    );
    let events = controller.hotplug_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, UsbIpHotplugKind::Export);
    assert_eq!(events[0].host.as_deref(), Some("192.0.2.10"));

    // A denied peer never reaches the command runner or state.
    let denied = controller
        .export_with_token("1-2", "192.0.2.11", Some(TOKEN), Some(FINGERPRINT))
        .unwrap_err();
    assert_eq!(denied.kind(), "usbip-unauthorized");
    assert!(!denied.to_string().contains(TOKEN));
    assert_eq!(controller.runner().calls().len(), 1);
    assert_eq!(controller.hotplug_events().len(), 1);
    assert!(!controller
        .known_state()
        .exported
        .iter()
        .any(|d| d.bus_id == "1-2"));
}

#[test]
fn tls_required_fails_closed_without_running_a_command() {
    let config =
        UsbIpConfig::server("127.0.0.1", DEFAULT_USBIP_PORT).with_tls(UsbIpTlsPolicy::Required);
    assert_eq!(
        config.validate().unwrap_err().kind(),
        "usbip-tls-unsupported"
    );
    let status = config.tls_status();
    assert!(!status.available);
    assert!(status.reason.contains("terminator"));

    let controller = make_controller(config);
    // Every enabled operation validates first and therefore fails closed.
    assert_eq!(
        controller.export("1-1").unwrap_err().kind(),
        "usbip-tls-unsupported"
    );
    assert_eq!(
        controller.attach("192.0.2.10", "1-1").unwrap_err().kind(),
        "usbip-tls-unsupported"
    );
    assert!(controller.runner().calls().is_empty());
    assert!(controller.hotplug_events().is_empty());

    // Disabled remains the default and keeps the historical behaviour.
    let default = UsbIpConfig::server("127.0.0.1", DEFAULT_USBIP_PORT);
    assert_eq!(default.tls, UsbIpTlsPolicy::Disabled);
    assert!(default.validate().is_ok());
}

#[test]
fn hotplug_log_drops_oldest_events_and_keeps_sequences() {
    let mut log = UsbIpHotplugLog::new(2);
    assert_eq!(log.capacity(), 2);
    log.record(UsbIpHotplugKind::Export, "1-1", None, None);
    log.record(
        UsbIpHotplugKind::Attach,
        "2-1",
        Some("192.0.2.10".into()),
        None,
    );
    log.record(
        UsbIpHotplugKind::Detach,
        "2-1",
        Some("192.0.2.10".into()),
        Some(1),
    );
    assert_eq!(log.len(), 2);
    let events = log.events();
    assert_eq!(events[0].sequence, 2);
    assert_eq!(events[0].kind, UsbIpHotplugKind::Attach);
    assert_eq!(events[1].sequence, 3);
    assert_eq!(events[1].kind.as_str(), "detach");
    assert_eq!(log.latest().unwrap().port, Some(1));
    let drained = log.drain();
    assert_eq!(drained.len(), 2);
    assert!(log.is_empty());

    // Capacity is clamped to the supported bounds.
    assert_eq!(UsbIpHotplugLog::new(0).capacity(), 1);
    assert_eq!(
        UsbIpHotplugLog::new(usize::MAX).capacity(),
        MAX_USBIP_HOTPLUG_EVENTS
    );
}

#[test]
fn controller_records_export_attach_and_detach_events() {
    let controller = make_controller(UsbIpConfig::server("127.0.0.1", DEFAULT_USBIP_PORT));
    for _ in 0..4 {
        controller.runner().push_ok();
    }
    controller.export("1-1").unwrap();
    controller.unexport("1-1").unwrap();
    controller.attach("192.0.2.10", "2-1").unwrap();
    controller.detach(1).unwrap();

    let events = controller.hotplug_events();
    let kinds: Vec<_> = events.iter().map(|event| event.kind).collect();
    assert_eq!(
        kinds,
        vec![
            UsbIpHotplugKind::Export,
            UsbIpHotplugKind::Unexport,
            UsbIpHotplugKind::Attach,
            UsbIpHotplugKind::Detach,
        ]
    );
    assert_eq!(
        events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert_eq!(events[2].bus_id, "2-1");
    assert_eq!(events[2].host.as_deref(), Some("192.0.2.10"));
    assert_eq!(
        events[3].port,
        Some(1),
        "detach records the requested local port"
    );

    let drained = controller.take_hotplug_events();
    assert_eq!(drained.len(), 4);
    assert!(controller.hotplug_events().is_empty());
    assert_eq!(controller.hotplug_capacity(), MAX_USBIP_HOTPLUG_EVENTS);
}

#[test]
fn hotplug_and_pairing_metadata_are_serializable() {
    let grant = UsbIpAuthorization {
        allowed_hosts: vec!["192.0.2.10".to_owned()],
        allowed_keys: vec![UsbIpKey::new(FINGERPRINT).unwrap()],
        allowed_tokens: Vec::new(),
    }
    .authorize_peer("192.0.2.10", None, Some(FINGERPRINT))
    .unwrap();
    let json = serde_json::to_value(&grant).unwrap();
    assert_eq!(json["host"], "192.0.2.10");
    assert_eq!(json["key"]["fingerprint"], FINGERPRINT);
    assert!(json.get("token_presented").is_some());
    assert!(!json.to_string().contains(TOKEN));

    let mut log = UsbIpHotplugLog::new(4);
    log.record(
        UsbIpHotplugKind::Attach,
        "2-1",
        Some("192.0.2.10".into()),
        None,
    );
    let json = serde_json::to_value(log.events()).unwrap();
    assert_eq!(json[0]["kind"], "attach");
    assert_eq!(json[0]["sequence"], 1);
    assert_eq!(json[0]["bus_id"], "2-1");
}
