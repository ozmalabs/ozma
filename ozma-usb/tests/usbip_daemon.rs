// SPDX-License-Identifier: AGPL-3.0-only
//! In-process USB/IP daemon tests.
//!
//! A scripted in-process kernel backend records calls; no `usbip`, kernel
//! module, socket, or device is touched.

use std::sync::{Arc, Mutex};

use ozma_usb::{
    NoUsbIpKernel, UsbError, UsbIpAuthorization, UsbIpDaemon, UsbIpDaemonConfig, UsbIpDaemonState,
    UsbIpHotplugKind, UsbIpKernelBackend, UsbIpKey, UsbIpTlsPolicy, UsbIpToken, DEFAULT_USBIP_PORT,
};

const TOKEN: &str = "ozma-peer-token-0123456789";
const FINGERPRINT: &str = "SHA256:ab:cd:ef:01:23:45:67";

#[derive(Default)]
struct FakeKernel {
    calls: Mutex<Vec<String>>,
}

impl FakeKernel {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn record(&self, call: impl Into<String>) {
        self.calls.lock().unwrap().push(call.into());
    }
}

impl UsbIpKernelBackend for FakeKernel {
    fn available(&self) -> bool {
        true
    }

    fn export(&self, bus_id: &str) -> Result<(), UsbError> {
        self.record(format!("export {bus_id}"));
        Ok(())
    }

    fn unexport(&self, bus_id: &str) -> Result<(), UsbError> {
        self.record(format!("unexport {bus_id}"));
        Ok(())
    }

    fn attach(&self, host: &str, bus_id: &str) -> Result<u16, UsbError> {
        self.record(format!("attach {host} {bus_id}"));
        Ok(7)
    }

    fn detach(&self, port: u16) -> Result<(), UsbError> {
        self.record(format!("detach {port}"));
        Ok(())
    }
}

/// A backend that attaches but cannot detach, to exercise the typed kernel
/// requirement after an attachment exists.
#[derive(Default)]
struct AttachOnlyKernel {
    calls: Mutex<Vec<String>>,
}

impl UsbIpKernelBackend for AttachOnlyKernel {
    fn available(&self) -> bool {
        true
    }

    fn attach(&self, host: &str, bus_id: &str) -> Result<u16, UsbError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("attach {host} {bus_id}"));
        Ok(3)
    }
}

fn paired_config() -> UsbIpDaemonConfig {
    let authorization = UsbIpAuthorization {
        allowed_hosts: vec!["192.0.2.10".to_owned()],
        allowed_keys: vec![UsbIpKey::new(FINGERPRINT).unwrap()],
        allowed_tokens: vec![UsbIpToken::new(TOKEN).unwrap()],
    };
    UsbIpDaemonConfig::server("127.0.0.1", DEFAULT_USBIP_PORT).with_authorization(authorization)
}

fn with_fake(config: UsbIpDaemonConfig) -> (UsbIpDaemon, Arc<FakeKernel>) {
    let fake = Arc::new(FakeKernel::default());
    let kernel: Arc<dyn UsbIpKernelBackend> = fake.clone();
    (UsbIpDaemon::new(config).unwrap().with_kernel(kernel), fake)
}

#[test]
fn authorized_attach_reaches_the_kernel_and_denied_attach_does_not() {
    let (daemon, kernel) = with_fake(paired_config());
    daemon.start().unwrap();
    assert!(daemon.is_running());
    assert!(daemon.status().kernel_available);

    let attachment = daemon
        .attach("192.0.2.10", "1-1", Some(TOKEN), Some(FINGERPRINT))
        .unwrap();
    assert_eq!(attachment.port, Some(7));
    assert_eq!(attachment.host, "192.0.2.10");
    assert_eq!(kernel.calls(), vec!["attach 192.0.2.10 1-1"]);
    assert_eq!(daemon.status().attached.len(), 1);
    let events = daemon.hotplug_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, UsbIpHotplugKind::Attach);
    assert_eq!(events[0].port, Some(7));

    let rejected = [
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
    for (host, token, fingerprint, label) in rejected {
        let error = daemon.attach(host, "1-2", token, fingerprint).unwrap_err();
        assert_eq!(error.kind(), "usbip-unauthorized", "{label}");
        if let Some(token) = token {
            assert!(!error.to_string().contains(token), "{label} leaked a token");
        }
        if let Some(fingerprint) = fingerprint {
            assert!(
                !error.to_string().contains(fingerprint),
                "{label} leaked a fingerprint"
            );
        }
    }
    // Only the authorized call reached the kernel and changed state.
    assert_eq!(kernel.calls(), vec!["attach 192.0.2.10 1-1"]);
    assert_eq!(daemon.status().attached.len(), 1);

    // The daemon/status shapes never contain the raw token or fingerprint.
    let status = serde_json::to_string(&daemon.status()).unwrap();
    let debug = format!("{daemon:?}");
    for shape in [status, debug] {
        assert!(!shape.contains(TOKEN));
        assert!(!shape.contains(FINGERPRINT));
    }
}

#[test]
fn connections_are_bounded_and_stop_clears_them() {
    let config = UsbIpDaemonConfig::server("127.0.0.1", DEFAULT_USBIP_PORT).with_limits(2, 4);
    let daemon = UsbIpDaemon::new(config).unwrap();
    assert_eq!(
        daemon
            .accept_connection("192.0.2.20", None, None)
            .unwrap_err()
            .kind(),
        "usbip-daemon-stopped"
    );
    daemon.start().unwrap();

    let first = daemon.accept_connection("192.0.2.20", None, None).unwrap();
    let second = daemon.accept_connection("192.0.2.21", None, None).unwrap();
    assert_eq!((first.id, second.id), (1, 2));
    assert_eq!(daemon.connections().len(), 2);
    assert_eq!(
        daemon
            .accept_connection("192.0.2.22", None, None)
            .unwrap_err()
            .kind(),
        "usbip-connection-limit"
    );

    daemon.close_connection(first.id).unwrap();
    let third = daemon.accept_connection("192.0.2.22", None, None).unwrap();
    assert_eq!(third.id, 3);
    assert_eq!(daemon.connections().len(), 2);

    daemon.stop().unwrap();
    assert!(!daemon.is_running());
    assert_eq!(daemon.status().state, UsbIpDaemonState::Stopped);
    assert!(daemon.connections().is_empty());
    assert_eq!(
        daemon
            .accept_connection("192.0.2.20", None, None)
            .unwrap_err()
            .kind(),
        "usbip-daemon-stopped"
    );
}

#[test]
fn export_attach_detach_unexport_cleanup_and_bounded_events() {
    let config = UsbIpDaemonConfig::server("127.0.0.1", DEFAULT_USBIP_PORT)
        .with_authorization(UsbIpAuthorization {
            allowed_hosts: vec!["192.0.2.10".to_owned()],
            allowed_keys: Vec::new(),
            allowed_tokens: vec![UsbIpToken::new(TOKEN).unwrap()],
        })
        .with_limits(4, 3);
    let (daemon, kernel) = with_fake(config);
    daemon.start().unwrap();

    daemon.export("1-1").unwrap();
    daemon
        .export_with_token("1-2", "192.0.2.10", Some(TOKEN), None)
        .unwrap();
    assert_eq!(daemon.list().unwrap().len(), 2);

    // A denied peer never exports.
    assert_eq!(
        daemon
            .export_with_token("1-3", "192.0.2.11", Some(TOKEN), None)
            .unwrap_err()
            .kind(),
        "usbip-unauthorized"
    );
    assert_eq!(kernel.calls().len(), 2);
    assert_eq!(daemon.list().unwrap().len(), 2);

    let attachment = daemon
        .attach_with_token("192.0.2.10", "2-1", Some(TOKEN))
        .unwrap();
    daemon.detach(attachment.port.unwrap()).unwrap();
    assert!(daemon.status().attached.is_empty());
    daemon.unbind("1-2").unwrap();
    assert_eq!(daemon.list().unwrap().len(), 1);

    // Detach/unexport reached the kernel.
    let calls = kernel.calls();
    assert!(calls.contains(&"attach 192.0.2.10 2-1".to_owned()));
    assert!(calls.contains(&"detach 7".to_owned()));
    assert!(calls.contains(&"unexport 1-2".to_owned()));

    // Five transitions were recorded but the log keeps only the newest three.
    let events = daemon.hotplug_events();
    assert_eq!(events.len(), 3);
    assert_eq!(daemon.hotplug_capacity(), 3);
    assert_eq!(
        events.iter().map(|event| event.kind).collect::<Vec<_>>(),
        vec![
            UsbIpHotplugKind::Attach,
            UsbIpHotplugKind::Detach,
            UsbIpHotplugKind::Unexport,
        ]
    );
    let sequences: Vec<u64> = events.iter().map(|event| event.sequence).collect();
    assert_eq!(sequences, vec![3, 4, 5]);
    assert!(daemon.take_hotplug_events().len() == 3);
    assert!(daemon.hotplug_events().is_empty());
}

#[test]
fn kernel_required_paths_are_typed_unsupported() {
    let daemon =
        UsbIpDaemon::new(UsbIpDaemonConfig::server("127.0.0.1", DEFAULT_USBIP_PORT)).unwrap();
    assert!(!daemon.status().kernel_available);
    daemon.start().unwrap();

    for error in [
        daemon.export("1-1").unwrap_err(),
        daemon.attach("192.0.2.10", "1-1", None, None).unwrap_err(),
        daemon.unexport("1-1").unwrap_err(),
    ] {
        assert_eq!(error.kind(), "usbip-kernel-required");
        assert!(error.to_string().contains("vhci_hcd"));
    }
    // No fabricated state and no hotplug events.
    assert!(daemon.status().exported.is_empty());
    assert!(daemon.status().attached.is_empty());
    assert!(daemon.hotplug_events().is_empty());
    // The explicit default backend exists as a named type too.
    assert!(!NoUsbIpKernel.available());

    // With a backend that can attach but not detach, the attachment is kept
    // and detach is a typed kernel requirement (never a false cleanup).
    let kernel: Arc<dyn UsbIpKernelBackend> = Arc::new(AttachOnlyKernel::default());
    let daemon = UsbIpDaemon::new(UsbIpDaemonConfig::server("127.0.0.1", DEFAULT_USBIP_PORT))
        .unwrap()
        .with_kernel(kernel);
    daemon.start().unwrap();
    let attachment = daemon.attach("192.0.2.10", "1-1", None, None).unwrap();
    assert_eq!(attachment.port, Some(3));
    assert_eq!(
        daemon.detach(3).unwrap_err().kind(),
        "usbip-kernel-required"
    );
    assert_eq!(
        daemon.status().attached.len(),
        1,
        "failed detach keeps state"
    );
}

#[test]
fn tls_required_fails_closed_and_is_never_faked() {
    let config = UsbIpDaemonConfig::server("127.0.0.1", DEFAULT_USBIP_PORT)
        .with_tls(UsbIpTlsPolicy::Required);
    assert_eq!(
        config.validate().unwrap_err().kind(),
        "usbip-tls-unsupported"
    );
    assert_eq!(
        UsbIpDaemon::new(config.clone()).unwrap_err().kind(),
        "usbip-tls-unsupported"
    );
    let status = config.tls_status();
    assert!(!status.available);
    assert!(status.reason.contains("terminator"));
    assert!(status.reason.contains("TLS"));

    let config = UsbIpDaemonConfig::server("127.0.0.1", DEFAULT_USBIP_PORT);
    let daemon = UsbIpDaemon::new(config).unwrap();
    daemon.start().unwrap();
    let status = daemon.status();
    assert_eq!(status.state, UsbIpDaemonState::Listening);
    assert!(
        !status.tls.available,
        "the daemon never claims in-process TLS"
    );
}

#[test]
fn invalid_configuration_is_rejected_before_listening() {
    let mut config = UsbIpDaemonConfig::server("127.0.0.1", DEFAULT_USBIP_PORT);
    config.port = 0;
    assert_eq!(
        UsbIpDaemon::new(config).unwrap_err().kind(),
        "invalid-usbip"
    );

    let zero_connections =
        UsbIpDaemonConfig::server("127.0.0.1", DEFAULT_USBIP_PORT).with_limits(0, 4);
    assert_eq!(
        UsbIpDaemon::new(zero_connections).unwrap_err().kind(),
        "invalid-usbip"
    );
    let huge_events =
        UsbIpDaemonConfig::server("127.0.0.1", DEFAULT_USBIP_PORT).with_limits(1, 1000);
    assert_eq!(
        UsbIpDaemon::new(huge_events).unwrap_err().kind(),
        "invalid-usbip"
    );
}
