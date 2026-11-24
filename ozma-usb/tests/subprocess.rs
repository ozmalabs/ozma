// SPDX-License-Identifier: AGPL-3.0-only
//! USB/IP subprocess controller and argv-only command runner.

use std::sync::Mutex;

use ozma_usb::{
    CommandOutput, CommandRunner, CommandSpec, SubprocessUsbController, SystemCommandRunner,
    UsbIpControl,
};

#[derive(Debug, Default)]
struct RecordingRunner {
    calls: Mutex<Vec<CommandSpec>>,
    stdout: String,
}

impl RecordingRunner {
    fn with_stdout(stdout: impl Into<String>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            stdout: stdout.into(),
        }
    }

    fn calls(&self) -> Vec<CommandSpec> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&self, command: &CommandSpec) -> Result<CommandOutput, ozma_usb::UsbError> {
        self.calls.lock().unwrap().push(command.clone());
        Ok(CommandOutput::success(self.stdout.clone()))
    }
}

#[test]
fn command_spec_keeps_arguments_separate() {
    let command = CommandSpec::new("usbip", ["attach", "-r", "host name"]);
    assert_eq!(
        command.argv(),
        vec!["usbip", "attach", "-r", "host name"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    );
}

#[test]
fn usbip_operations_use_exact_argv() {
    let runner = RecordingRunner::default();
    let controller = SubprocessUsbController::new(runner, "usbip");

    controller.bind("1-1").unwrap();
    controller.unbind("1-1").unwrap();
    controller.attach("192.0.2.10", "1-1.2").unwrap();
    controller.detach("3240").unwrap();

    let calls = controller.runner().calls();
    assert_eq!(
        calls,
        vec![
            CommandSpec::new("usbip", ["bind", "-b", "1-1"]),
            CommandSpec::new("usbip", ["unbind", "-b", "1-1"]),
            CommandSpec::new("usbip", ["attach", "-r", "192.0.2.10", "-b", "1-1.2"]),
            CommandSpec::new("usbip", ["detach", "-p", "3240"]),
        ]
    );
}

#[test]
fn invalid_usbip_values_are_rejected_before_running() {
    let runner = RecordingRunner::default();
    let controller = SubprocessUsbController::new(runner, "usbip");

    for bus_id in ["", "1 1", "--help", "1/1", "abc"] {
        assert_eq!(
            controller.bind(bus_id).unwrap_err().kind(),
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
    for port in ["", "0", "-1", "65536", "abc"] {
        assert_eq!(
            controller.detach(port).unwrap_err().kind(),
            "invalid-usbip",
            "port {port:?}"
        );
    }

    assert!(controller.runner().calls().is_empty());
}

#[test]
fn list_local_parses_usbip_output() {
    let output = "\
 - busid 1-1 (046d:c52b)
   Logitech, Inc. : Unifying Receiver (046d:c52b)

 - busid 1-2.3 (1234:abcd)
   Example device
";
    let devices = SubprocessUsbController::<RecordingRunner>::parse_list(output);
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0].bus_id, "1-1");
    assert_eq!(devices[0].vendor_id, Some(0x046d));
    assert_eq!(devices[0].product_id, Some(0xc52b));
    assert!(devices[0].description.contains("Unifying Receiver"));
    assert_eq!(devices[1].bus_id, "1-2.3");
    assert_eq!(devices[1].vendor_id, Some(0x1234));
    assert_eq!(devices[1].product_id, Some(0xabcd));
}

#[test]
fn list_local_uses_the_runner() {
    let runner = RecordingRunner::with_stdout(" - busid 1-1 (046d:c52b)\n   Receiver\n");
    let controller = SubprocessUsbController::new(runner, "/usr/bin/usbip");
    let devices = controller.list_local().unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(
        controller.runner().calls(),
        vec![CommandSpec::new("/usr/bin/usbip", ["list", "-l"])]
    );
}

#[test]
fn system_runner_reports_a_missing_executable() {
    let runner = SystemCommandRunner;
    let error = runner
        .run(&CommandSpec::new(
            "ozma-usb-definitely-not-installed-1017",
            Vec::<String>::new(),
        ))
        .unwrap_err();
    assert_eq!(error.kind(), "missing-command");
}
