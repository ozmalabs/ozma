// SPDX-License-Identifier: AGPL-3.0-only
//! USB/IP configuration, argv construction, state, and typed failures.
//!
//! All tests use a scripted runner; no real `usbip`, kernel module, or socket
//! is touched.

use std::collections::VecDeque;
use std::sync::Mutex;

use ozma_usb::{
    CommandOutput, CommandRunner, CommandSpec, SubprocessUsbIpController, UsbError, UsbIpConfig,
    UsbIpController, UsbIpDevice, UsbIpKey, DEFAULT_USBIP_PORT,
};

#[derive(Debug, Default)]
struct QueueRunner {
    calls: Mutex<Vec<CommandSpec>>,
    responses: Mutex<VecDeque<Result<CommandOutput, UsbError>>>,
}

impl QueueRunner {
    fn push_ok(&self, stdout: &str) {
        self.responses
            .lock()
            .unwrap()
            .push_back(Ok(CommandOutput::success(stdout)));
    }

    fn push_err(&self, error: UsbError) {
        self.responses.lock().unwrap().push_back(Err(error));
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

#[test]
fn commands_use_argv_and_state_tracks_attach_detach() {
    let config = UsbIpConfig::server("127.0.0.1", DEFAULT_USBIP_PORT);
    let controller = make_controller(config);
    controller.runner().push_ok("");
    controller.runner().push_ok("");

    controller.export("1-1").unwrap();
    let attachment = controller.attach("192.0.2.10", "1-2").unwrap();
    assert_eq!(attachment.host, "192.0.2.10");
    assert_eq!(attachment.bus_id, "1-2");
    assert!(controller.known_state().is_attached("1-2"));

    controller.runner().push_ok(
        "\
Imported USB devices
====================
Port 01: <Port in Use> at High Speed(480Mbps)
     Logitech, Inc. : Unifying Receiver (046d:c52b)
     3-1 -> usbip://192.0.2.10:3240/1-2
",
    );
    let state = controller.status().unwrap();
    assert!(state.enabled);
    assert_eq!(state.attached.len(), 1);
    assert_eq!(state.attached[0].host, "192.0.2.10");
    assert_eq!(state.attached[0].bus_id, "1-2");
    assert_eq!(state.attached[0].port, Some(1));
    assert_eq!(state.attached[0].vendor_id, Some(0x046d));
    assert_eq!(state.attached[0].product_id, Some(0xc52b));

    controller.runner().push_ok("");
    controller.detach(1).unwrap();
    assert!(controller.known_state().attached.is_empty());

    assert_eq!(
        controller.runner().calls(),
        vec![
            CommandSpec::new("usbip", ["bind", "-b", "1-1"]),
            CommandSpec::new("usbip", ["attach", "-r", "192.0.2.10", "-b", "1-2"]),
            CommandSpec::new("usbip", ["port"]),
            CommandSpec::new("usbip", ["detach", "-p", "1"]),
        ]
    );
}

#[test]
fn list_parses_local_devices_and_uses_the_runner() {
    let config = UsbIpConfig::server("127.0.0.1", 3240);
    let controller = make_controller(config);
    controller.runner().push_ok(
        "\
 - busid 1-1 (046d:c52b)
   Logitech, Inc. : Unifying Receiver (046d:c52b)
 - busid 1-2.3 (1234:abcd)
   Example device
",
    );
    let devices = controller.list().unwrap();
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0].bus_id, "1-1");
    assert_eq!(devices[0].vid_pid(), Some((0x046d, 0xc52b)));
    assert!(devices[0].host.is_none());
    assert_eq!(devices[1].bus_id, "1-2.3");
    assert_eq!(
        controller.runner().calls(),
        vec![CommandSpec::new("usbip", ["list", "-l"])]
    );
}

#[test]
fn invalid_values_are_rejected_before_running() {
    let config = UsbIpConfig::server("127.0.0.1", 3240);
    let controller = make_controller(config);

    for bus_id in ["", "1 1", "--help", "1/1", "abc"] {
        assert_eq!(
            controller.export(bus_id).unwrap_err().kind(),
            "invalid-usbip",
            "bus id {bus_id:?}"
        );
    }
    for host in ["", "host name", "-oProxyCommand=x", "host;rm"] {
        assert_eq!(
            controller.attach(host, "1-1").unwrap_err().kind(),
            "invalid-usbip",
            "host {host:?}"
        );
    }
    assert_eq!(controller.detach(0).unwrap_err().kind(), "invalid-usbip");
    assert!(controller.runner().calls().is_empty());
}

#[test]
fn disabled_configuration_is_a_typed_failure() {
    let controller = make_controller(UsbIpConfig::disabled());
    assert_eq!(
        controller.export("1-1").unwrap_err().kind(),
        "usbip-disabled"
    );
    assert_eq!(
        controller.attach("192.0.2.10", "1-1").unwrap_err().kind(),
        "usbip-disabled"
    );
    assert_eq!(controller.detach(1).unwrap_err().kind(), "usbip-disabled");
    // Status on a disabled feature is a real disabled snapshot, not a command.
    assert!(!controller.status().unwrap().enabled);
    assert!(controller.runner().calls().is_empty());
}

#[test]
fn missing_binary_and_nonzero_exit_are_typed() {
    let config = UsbIpConfig::server("127.0.0.1", 3240);
    let controller = make_controller(config);
    controller.runner().push_err(UsbError::MissingCommand {
        command: "usbip".to_owned(),
        remediation: "install usbip".to_owned(),
    });
    assert_eq!(controller.list().unwrap_err().kind(), "missing-command");

    let failing = make_controller(UsbIpConfig::server("127.0.0.1", 3240));
    failing.runner().push_err(UsbError::CommandFailed {
        command: "usbip attach -r 192.0.2.10 -b 1-1".to_owned(),
        status: 1,
        stderr: "device busy".to_owned(),
    });
    let error = failing.attach("192.0.2.10", "1-1").unwrap_err();
    assert_eq!(error.kind(), "command-failed");
    assert!(error.to_string().contains("device busy"));
}

#[test]
fn config_validates_host_port_devices_and_pairing_metadata() {
    let mut config = UsbIpConfig::server("127.0.0.1", 3240);
    config.port = 0;
    assert_eq!(config.validate().unwrap_err().kind(), "invalid-usbip");

    let config = UsbIpConfig::server("bad host", 3240);
    assert_eq!(config.validate().unwrap_err().kind(), "invalid-usbip");

    let config = UsbIpConfig::server("127.0.0.1", 3240)
        .with_device(UsbIpDevice::remote("192.0.2.10", "1-1").with_ids(0x046d, 0xc52b));
    assert!(config.validate().is_ok());

    let config = UsbIpConfig::server("127.0.0.1", 3240)
        .with_device(UsbIpDevice::remote("192.0.2.10", "1-1"))
        .with_device(UsbIpDevice::remote("192.0.2.10", "1-1"));
    assert_eq!(config.validate().unwrap_err().kind(), "invalid-usbip");

    let mut bad_key = UsbIpConfig::server("127.0.0.1", 3240);
    bad_key.authorization = Some(ozma_usb::UsbIpAuthorization {
        allowed_hosts: vec!["192.0.2.10".to_owned()],
        allowed_keys: vec![UsbIpKey {
            fingerprint: "not a fingerprint".to_owned(),
            label: None,
        }],
    });
    assert_eq!(bad_key.validate().unwrap_err().kind(), "invalid-usbip");

    let good_key = UsbIpKey::new("SHA256:ab:cd:ef:01:23:45:67").unwrap();
    let mut good = UsbIpConfig::server("127.0.0.1", 3240);
    good.authorization = Some(ozma_usb::UsbIpAuthorization {
        allowed_hosts: vec!["192.0.2.10".to_owned()],
        allowed_keys: vec![good_key],
    });
    assert!(good.validate().is_ok());
    assert!(good
        .authorization
        .as_ref()
        .unwrap()
        .host_allowed("192.0.2.10"));
}

#[test]
fn ports_parse_ipv4_and_ipv6_urls() {
    let parsed = SubprocessUsbIpController::<QueueRunner>::parse_ports(
        "\
Imported USB devices
Port 01: <Port in Use>
     1-1 -> usbip://192.0.2.10:3240/1-1
Port 02: <Port in Use>
     2-1 -> usbip://[2001:db8::1]:3240/2-1
",
    );
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].host, "192.0.2.10");
    assert_eq!(parsed[0].port, Some(1));
    assert_eq!(parsed[1].host, "2001:db8::1");
    assert_eq!(parsed[1].bus_id, "2-1");
}
