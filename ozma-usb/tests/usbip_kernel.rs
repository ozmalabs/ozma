// SPDX-License-Identifier: AGPL-3.0-only
//! Linux USB/IP host backend tests.
//!
//! Every test uses a fake sysfs tree and a scripted `CommandRunner`; no `usbip`
//! executable, kernel module, `/sys` entry, or device is touched.

use std::collections::VecDeque;
use std::fs;
use std::sync::Mutex;

use ozma_usb::{
    CommandOutput, CommandRunner, CommandSpec, LinuxUsbIpHostBackend, UsbError, UsbIpAuthorization,
    UsbIpKernelBackend, UsbIpKernelSysfs, UsbIpToken,
};

#[derive(Clone, Debug)]
enum FakeFailure {
    Missing,
    Failed { status: i32, stderr: String },
}

#[derive(Debug, Default)]
struct FakeRunner {
    calls: Mutex<Vec<CommandSpec>>,
    responses: Mutex<VecDeque<Result<CommandOutput, FakeFailure>>>,
}

impl FakeRunner {
    fn with_responses(responses: Vec<Result<CommandOutput, FakeFailure>>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into()),
        }
    }

    fn calls(&self) -> Vec<CommandSpec> {
        self.calls.lock().unwrap().clone()
    }

    fn call_strings(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .map(|command| command.argv().join(" "))
            .collect()
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, command: &CommandSpec) -> Result<CommandOutput, UsbError> {
        self.calls.lock().unwrap().push(command.clone());
        match self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(CommandOutput::success("")))
        {
            Ok(output) => Ok(output),
            Err(FakeFailure::Missing) => Err(UsbError::MissingCommand {
                command: command.program.clone(),
                remediation: "test".to_owned(),
            }),
            Err(FakeFailure::Failed { status, stderr }) => Err(UsbError::CommandFailed {
                command: command.argv().join(" "),
                status,
                stderr,
            }),
        }
    }
}

fn fake_sysfs() -> (tempfile::TempDir, UsbIpKernelSysfs) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("module/vhci_hcd")).unwrap();
    fs::create_dir_all(root.join("module/usbip_host")).unwrap();
    fs::create_dir_all(root.join("devices/platform/vhci_hcd.0")).unwrap();
    fs::write(root.join("devices/platform/vhci_hcd.0/attach"), "").unwrap();
    fs::create_dir_all(root.join("bus/usb/drivers/usbip-host")).unwrap();
    let sysfs = UsbIpKernelSysfs::with_root(root);
    (temp, sysfs)
}

fn bind_device(sysfs: &UsbIpKernelSysfs, bus_id: &str) {
    let path = sysfs.host_driver_root().join(bus_id);
    fs::create_dir_all(path).unwrap();
}

fn backend(runner: FakeRunner, sysfs: UsbIpKernelSysfs) -> LinuxUsbIpHostBackend<FakeRunner> {
    LinuxUsbIpHostBackend::new(runner, "/usr/bin/usbip", sysfs)
}

fn token() -> UsbIpToken {
    UsbIpToken::new("ozma-peer-token-0123456789").unwrap()
}

fn authorization() -> UsbIpAuthorization {
    UsbIpAuthorization {
        allowed_hosts: vec!["192.0.2.10".to_owned()],
        allowed_keys: vec![],
        allowed_tokens: vec![token()],
    }
}

fn port_listing(port: u16, host: &str, bus_id: &str) -> String {
    format!(
        "Imported USB devices\n====================\nPort {port:02}: <Port in Use> at High Speed(480Mbps)\n      (046d:c52b)\n          usbip://{host}:3240/{bus_id}\n"
    )
}

#[test]
fn available_requires_both_modules_and_the_vhci_attach_attribute() {
    let (_temp, sysfs) = fake_sysfs();
    let backend = backend(FakeRunner::default(), sysfs.clone());
    assert!(backend.available());
    assert!(backend.sysfs().vhci_present());
    assert!(backend.sysfs().host_module_present());

    fs::remove_dir_all(sysfs.module_path("vhci_hcd")).unwrap();
    assert!(!backend.available());
    fs::create_dir_all(sysfs.module_path("vhci_hcd")).unwrap();
    fs::remove_file(sysfs.platform_root().join("vhci_hcd.0/attach")).unwrap();
    assert!(!backend.available());
}

#[test]
fn export_verifies_the_sysfs_binding_and_unexport_uses_unbind() {
    let (_temp, sysfs) = fake_sysfs();
    bind_device(&sysfs, "1-1");
    let runner = FakeRunner::default();
    let backend = backend(runner, sysfs.clone());

    backend.export("1-1").unwrap();
    backend.unexport("1-1").unwrap();
    assert_eq!(
        backend.runner().call_strings(),
        vec![
            "/usr/bin/usbip bind -b 1-1".to_owned(),
            "/usr/bin/usbip unbind -b 1-1".to_owned(),
        ]
    );
}

#[test]
fn export_rolls_back_when_the_kernel_did_not_create_the_binding() {
    let (_temp, sysfs) = fake_sysfs();
    let runner = FakeRunner::default();
    let backend = backend(runner, sysfs);

    let error = backend.export("1-1").unwrap_err();
    assert_eq!(error.kind(), "usbip-kernel-required");
    assert!(error.to_string().contains("rolled back"));
    assert_eq!(
        backend.runner().call_strings(),
        vec![
            "/usr/bin/usbip bind -b 1-1".to_owned(),
            "/usr/bin/usbip unbind -b 1-1".to_owned(),
        ]
    );
}

#[test]
fn attach_parses_the_port_printed_by_usbip() {
    let (_temp, sysfs) = fake_sysfs();
    let runner = FakeRunner::with_responses(vec![Ok(CommandOutput::success(
        "usbip: info: using port 07\n",
    ))]);
    let backend = backend(runner, sysfs);

    assert_eq!(backend.attach("192.0.2.10", "1-1").unwrap(), 7);
    assert_eq!(
        backend.runner().call_strings(),
        vec!["/usr/bin/usbip attach -r 192.0.2.10 -b 1-1".to_owned()]
    );

    backend.detach(7).unwrap();
    assert_eq!(
        backend.runner().call_strings().last().unwrap(),
        "/usr/bin/usbip detach -p 7"
    );
}

#[test]
fn attach_parses_the_vhci_port_listing_when_output_is_quiet() {
    let (_temp, sysfs) = fake_sysfs();
    let runner = FakeRunner::with_responses(vec![
        Ok(CommandOutput::success("")),
        Ok(CommandOutput::success(port_listing(
            0,
            "192.0.2.10",
            "1-2.3",
        ))),
    ]);
    let backend = backend(runner, sysfs);

    assert_eq!(backend.attach("192.0.2.10", "1-2.3").unwrap(), 0);
    assert_eq!(
        backend.runner().call_strings(),
        vec![
            "/usr/bin/usbip attach -r 192.0.2.10 -b 1-2.3".to_owned(),
            "/usr/bin/usbip port".to_owned(),
        ]
    );
}

#[test]
fn attach_never_fabricates_a_missing_port() {
    let (_temp, sysfs) = fake_sysfs();
    let runner = FakeRunner::with_responses(vec![
        Ok(CommandOutput::success("")),
        Ok(CommandOutput::success("Imported USB devices\n")),
    ]);
    let backend = backend(runner, sysfs);

    let error = backend.attach("192.0.2.10", "1-1").unwrap_err();
    assert_eq!(error.kind(), "usbip-kernel-required");
    assert!(error.to_string().contains("no port was fabricated"));
    assert_eq!(backend.runner().call_strings().len(), 2);
}

#[test]
fn ports_lists_real_attachments() {
    let (_temp, sysfs) = fake_sysfs();
    let runner = FakeRunner::with_responses(vec![Ok(CommandOutput::success(port_listing(
        3,
        "192.0.2.10",
        "1-1",
    )))]);
    let backend = backend(runner, sysfs);

    let ports = backend.ports().unwrap();
    assert_eq!(ports.len(), 1);
    assert_eq!(ports[0].host, "192.0.2.10");
    assert_eq!(ports[0].bus_id, "1-1");
    assert_eq!(ports[0].port, Some(3));
    assert_eq!(ports[0].vendor_id, Some(0x046d));
}

#[test]
fn missing_module_is_typed_and_runs_no_command() {
    let (_temp, sysfs) = fake_sysfs();
    fs::remove_dir_all(sysfs.module_path("usbip_host")).unwrap();
    fs::remove_dir_all(sysfs.module_path("vhci_hcd")).unwrap();
    let runner = FakeRunner::default();
    let backend = backend(runner, sysfs);

    assert!(!backend.available());
    assert_eq!(
        backend.export("1-1").unwrap_err().kind(),
        "usbip-kernel-required"
    );
    assert_eq!(
        backend.attach("192.0.2.10", "1-1").unwrap_err().kind(),
        "usbip-kernel-required"
    );
    assert!(backend.runner().calls().is_empty());
}

#[test]
fn missing_sysfs_attach_attribute_is_typed() {
    let (_temp, sysfs) = fake_sysfs();
    fs::remove_dir_all(sysfs.platform_root()).unwrap();
    let backend = backend(FakeRunner::default(), sysfs);

    assert_eq!(
        backend.attach("192.0.2.10", "1-1").unwrap_err().kind(),
        "usbip-kernel-required"
    );
    assert!(backend.runner().calls().is_empty());
}

#[test]
fn missing_usbip_executable_is_a_kernel_requirement() {
    let (_temp, sysfs) = fake_sysfs();
    let runner = FakeRunner::with_responses(vec![Err(FakeFailure::Missing)]);
    let backend = backend(runner, sysfs);

    let error = backend.attach("192.0.2.10", "1-1").unwrap_err();
    assert_eq!(error.kind(), "usbip-kernel-required");
    assert!(error.to_string().contains("executable is unavailable"));
}

#[test]
fn command_failure_is_not_reported_as_success() {
    let (_temp, sysfs) = fake_sysfs();
    let runner = FakeRunner::with_responses(vec![Err(FakeFailure::Failed {
        status: 1,
        stderr: "device busy".to_owned(),
    })]);
    let backend = backend(runner, sysfs);

    let error = backend.attach("192.0.2.10", "1-1").unwrap_err();
    assert_eq!(error.kind(), "command-failed");
    assert!(error.to_string().contains("device busy"));
}

#[test]
fn authorization_denial_runs_no_command() {
    let (_temp, sysfs) = fake_sysfs();
    bind_device(&sysfs, "1-1");
    let runner = FakeRunner::default();
    let backend = backend(runner, sysfs).with_authorization(authorization());

    for error in [
        backend
            .export_for_peer(
                "1-1",
                "198.51.100.9",
                Some("ozma-peer-token-0123456789"),
                None,
            )
            .unwrap_err(),
        backend
            .export_for_peer("1-1", "192.0.2.10", Some("wrong-token-0123456789"), None)
            .unwrap_err(),
        backend
            .export_for_peer("1-1", "192.0.2.10", None, None)
            .unwrap_err(),
        backend
            .attach_authorized("198.51.100.9", "1-1", None, None)
            .unwrap_err(),
    ] {
        assert_eq!(error.kind(), "usbip-unauthorized");
        assert!(!error.to_string().contains("ozma-peer-token"));
        assert!(!error.to_string().contains("wrong-token"));
    }
    assert!(backend.runner().calls().is_empty());

    backend
        .export_for_peer(
            "1-1",
            "192.0.2.10",
            Some("ozma-peer-token-0123456789"),
            None,
        )
        .unwrap();
    assert_eq!(
        backend.runner().call_strings(),
        vec!["/usr/bin/usbip bind -b 1-1".to_owned()]
    );
}

#[test]
fn attach_authorized_uses_the_same_argv_after_pairing() {
    let (_temp, sysfs) = fake_sysfs();
    let runner = FakeRunner::with_responses(vec![Ok(CommandOutput::success(
        "usbip: info: using port 02\n",
    ))]);
    let mut authorization = authorization();
    authorization.allowed_keys =
        vec![ozma_usb::UsbIpKey::new("SHA256:ab:cd:ef:01:23:45:67").unwrap()];
    let backend = backend(runner, sysfs).with_authorization(authorization);

    backend
        .attach_authorized(
            "192.0.2.10",
            "1-1",
            Some("ozma-peer-token-0123456789"),
            Some("SHA256:ab:cd:ef:01:23:45:67"),
        )
        .unwrap();
    assert_eq!(
        backend.runner().call_strings(),
        vec!["/usr/bin/usbip attach -r 192.0.2.10 -b 1-1".to_owned()]
    );
}

#[test]
fn attach_authorized_requires_a_paired_fingerprint_when_configured() {
    let (_temp, sysfs) = fake_sysfs();
    let runner = FakeRunner::default();
    let mut authorization = authorization();
    authorization.allowed_keys =
        vec![ozma_usb::UsbIpKey::new("SHA256:ab:cd:ef:01:23:45:67").unwrap()];
    let backend = backend(runner, sysfs).with_authorization(authorization);

    let error = backend
        .attach_authorized(
            "192.0.2.10",
            "1-1",
            Some("ozma-peer-token-0123456789"),
            None,
        )
        .unwrap_err();
    assert_eq!(error.kind(), "usbip-unauthorized");
    assert!(backend.runner().calls().is_empty());
}

#[test]
fn invalid_values_are_rejected_before_sysfs_or_commands() {
    let (_temp, sysfs) = fake_sysfs();
    let runner = FakeRunner::default();
    let backend = backend(runner, sysfs);

    assert_eq!(
        backend.export("--help").unwrap_err().kind(),
        "invalid-usbip"
    );
    assert_eq!(
        backend.attach("host name", "1-1").unwrap_err().kind(),
        "invalid-usbip"
    );
    assert!(backend.runner().calls().is_empty());
}

#[test]
#[ignore = "requires a real /sys vhci_hcd/usbip-host and root privileges"]
fn real_kernel_bind_and_attach_are_environment_gated() {
    let Some(bus_id) = std::env::var("OZMA_USBIP_REAL_BUSID").ok() else {
        eprintln!("set OZMA_USBIP_REAL_BUSID to run the real kernel USB/IP test");
        return;
    };
    let backend = LinuxUsbIpHostBackend::system();
    assert!(backend.available(), "USB/IP kernel modules are absent");
    backend.export(&bus_id).unwrap();
    backend.unexport(&bus_id).unwrap();
}
