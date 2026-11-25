// SPDX-License-Identifier: AGPL-3.0-only
//! Typed FunctionFS device-state tests.
//!
//! These tests drive the state machine directly and through the in-memory
//! daemon core. Nothing mounts FunctionFS, opens a real endpoint, or binds a
//! UDC.

use ozma_usb::{
    FakeFunctionFsDaemon, FunctionFsConfig, FunctionFsDaemon, FunctionFsDeviceState,
    FunctionFsEndpoint, FunctionFsEvent, FunctionFsLifecycle, FunctionFsLifecycleConfig,
    FunctionFsMounter, FunctionFsStateMachine,
};

fn vendor_config() -> FunctionFsConfig {
    FunctionFsConfig::new("vendor0", 0x1209, 0x0001)
        .with_interface(0xff, 0x42, 0x01)
        .with_endpoint(FunctionFsEndpoint::bulk_out(1, 512))
        .with_endpoint(FunctionFsEndpoint::bulk_in(2, 512))
}

#[test]
fn state_machine_orders_events_and_exposes_udc_readiness() {
    let mut machine = FunctionFsStateMachine::new();
    assert_eq!(machine.state(), FunctionFsDeviceState::Unconfigured);
    assert!(!machine.udc_bind_ready());
    assert!(!machine.udc_unbind_ready());
    assert_eq!(
        machine.record_endpoint_event().unwrap_err().kind(),
        "invalid-functionfs-state"
    );

    machine.bind_descriptors().unwrap();
    assert_eq!(machine.state(), FunctionFsDeviceState::DescriptorsBound);
    assert!(machine.descriptors_bound());
    assert!(!machine.ep0_ready());
    assert!(!machine.udc_bind_ready());

    machine.mark_ep0_ready().unwrap();
    assert_eq!(machine.state(), FunctionFsDeviceState::Ep0Ready);
    assert!(machine.udc_bind_ready());
    assert!(!machine.is_running());

    // Enable without endpoint traffic is never "running".
    machine.enable().unwrap();
    assert_eq!(machine.state(), FunctionFsDeviceState::Enabled);
    assert!(machine.is_enabled());
    assert!(!machine.is_running());
    assert!(!machine.status().running);

    // A UDC bind records readiness but still does not imply endpoint traffic.
    machine.bind_udc().unwrap();
    assert!(machine.udc_bound());
    assert!(machine.udc_unbind_ready());
    assert!(!machine.udc_bind_ready());
    assert!(!machine.is_running());

    machine.record_endpoint_event().unwrap();
    assert_eq!(machine.state(), FunctionFsDeviceState::Running);
    assert_eq!(machine.status().endpoint_events, 1);
    assert!(machine.is_running());

    machine.disable().unwrap();
    assert_eq!(machine.state(), FunctionFsDeviceState::Disabled);
    assert!(!machine.status().enabled);
    assert!(!machine.status().running);
    assert_eq!(
        machine.record_endpoint_event().unwrap_err().kind(),
        "invalid-functionfs-state"
    );

    machine.record_unbind().unwrap();
    assert_eq!(machine.state(), FunctionFsDeviceState::Unbound);
    assert!(!machine.udc_bound());

    machine.reset();
    assert_eq!(machine.state(), FunctionFsDeviceState::Unconfigured);
}

#[test]
fn out_of_order_transitions_are_typed() {
    let mut machine = FunctionFsStateMachine::new();
    assert_eq!(
        machine.mark_ep0_ready().unwrap_err().kind(),
        "invalid-functionfs-state"
    );
    assert_eq!(
        machine.bind_udc().unwrap_err().kind(),
        "invalid-functionfs-state"
    );
    assert_eq!(
        machine.enable().unwrap_err().kind(),
        "invalid-functionfs-state"
    );
    assert_eq!(
        machine.disable().unwrap_err().kind(),
        "invalid-functionfs-state"
    );
    assert_eq!(
        machine.record_unbind().unwrap_err().kind(),
        "invalid-functionfs-state"
    );

    machine.bind_descriptors().unwrap();
    assert_eq!(
        machine.bind_udc().unwrap_err().kind(),
        "invalid-functionfs-state",
        "a UDC cannot bind before ep0 readiness"
    );
    assert_eq!(
        machine.enable().unwrap_err().kind(),
        "invalid-functionfs-state",
        "enable requires ep0 readiness"
    );
    machine.mark_ep0_ready().unwrap();
    machine.bind_udc().unwrap();
    assert_eq!(
        machine.bind_udc().unwrap_err().kind(),
        "invalid-functionfs-state",
        "a second UDC bind is rejected"
    );
}

#[test]
fn daemon_status_never_claims_running_before_endpoint_events() {
    let config = vendor_config();
    let descriptors = config.ep0_descriptors().unwrap();
    let mut daemon = FakeFunctionFsDaemon::new(config.clone()).unwrap();

    // Enable before descriptors is still a typed rejection.
    assert_eq!(
        daemon
            .handle_event(&config, FunctionFsEvent::Enable)
            .unwrap_err()
            .kind(),
        "invalid-functionfs-state"
    );

    daemon.write_descriptors(&config, &descriptors).unwrap();
    let state = daemon.status(&config).daemon.unwrap();
    assert_eq!(state.state, FunctionFsDeviceState::Ep0Ready);
    assert!(!state.enabled);
    assert!(!state.running);
    assert!(!state.live);
    assert_eq!(state.endpoint_events, 0);

    let outcome = daemon
        .handle_event(&config, FunctionFsEvent::Enable)
        .unwrap();
    assert_eq!(outcome.state, FunctionFsDeviceState::Enabled);
    assert!(!outcome.running);
    let state = daemon.status(&config).daemon.unwrap();
    assert!(state.enabled);
    assert!(state.live);
    assert!(!state.running);
    assert!(!daemon.core_mut().is_running());

    // Only a completed endpoint transfer reaches `running`.
    daemon.core_mut().write_endpoint(0x82, b"ping").unwrap();
    let state = daemon.status(&config).daemon.unwrap();
    assert_eq!(state.state, FunctionFsDeviceState::Running);
    assert!(state.running);
    assert_eq!(state.endpoint_events, 1);
    assert!(daemon.core_mut().is_running());

    daemon
        .handle_event(&config, FunctionFsEvent::Disable)
        .unwrap();
    let state = daemon.status(&config).daemon.unwrap();
    assert_eq!(state.state, FunctionFsDeviceState::Disabled);
    assert!(!state.enabled);
    assert!(!state.running);
    assert!(!state.live);

    daemon
        .handle_event(&config, FunctionFsEvent::Unbind)
        .unwrap();
    let state = daemon.status(&config).daemon.unwrap();
    assert_eq!(state.state, FunctionFsDeviceState::Unbound);
    assert!(!state.udc_bound);
}

#[test]
fn bind_and_unbind_events_track_udc_readiness() {
    let config = vendor_config();
    let descriptors = config.ep0_descriptors().unwrap();
    let mut daemon = FakeFunctionFsDaemon::new(config.clone()).unwrap();

    // A bind event before descriptor/ep0 readiness is rejected.
    assert_eq!(
        daemon
            .handle_event(&config, FunctionFsEvent::Bind)
            .unwrap_err()
            .kind(),
        "invalid-functionfs-state"
    );

    daemon.write_descriptors(&config, &descriptors).unwrap();
    let outcome = daemon.handle_event(&config, FunctionFsEvent::Bind).unwrap();
    assert_eq!(outcome.state, FunctionFsDeviceState::Ep0Ready);
    assert!(daemon.device_status().udc_bound);
    assert!(daemon.device_status().udc_unbind_ready());
    assert!(!daemon.device_status().udc_bind_ready());

    daemon
        .handle_event(&config, FunctionFsEvent::Enable)
        .unwrap();
    daemon
        .handle_event(&config, FunctionFsEvent::Unbind)
        .unwrap();
    let state = daemon.status(&config).daemon.unwrap();
    assert_eq!(state.state, FunctionFsDeviceState::Unbound);
    assert!(!state.udc_bound);
}

#[test]
fn lifecycle_helper_requires_ep0_readiness_before_udc_bind() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("usb_gadget");
    std::fs::create_dir(&root).unwrap();
    let mountpoint = temp.path().join("dev/ffs-vendor0");
    let program = temp.path().join("ffs-daemon");
    std::fs::write(&program, b"#!/bin/sh\nexit 0\n").unwrap();
    let marker = temp.path().join("ffs-ready");
    let function = vendor_config()
        .with_daemon(ozma_usb::FunctionFsDaemonSpec::new(&program).with_ready_marker(&marker));
    let config = FunctionFsLifecycleConfig::new("ozma-test", &root, "dummy_udc.0", function)
        .with_mountpoint(&mountpoint)
        .with_readiness(100, 10);
    let mut lifecycle = FunctionFsLifecycle::new(config)
        .unwrap()
        .with_mounter(ozma_usb::FakeFunctionFsMounter::new().with_endpoints([1, 2]))
        .with_clock(ozma_usb::ManualFunctionFsLifecycleClock::new(0));

    lifecycle.prepare().unwrap();
    lifecycle.mount().unwrap();
    let status = lifecycle.write_descriptors().unwrap();
    assert_eq!(status.device.state, FunctionFsDeviceState::DescriptorsBound);
    assert!(!status.udc_bind_ready());
    assert!(!status.running());
    assert_eq!(
        lifecycle.bind().unwrap_err().kind(),
        "invalid-functionfs-state"
    );
    assert_eq!(
        lifecycle.unbind().unwrap_err().kind(),
        "invalid-functionfs-state",
        "unbind without a bound UDC is rejected"
    );

    std::fs::write(&marker, b"ready\n").unwrap();
    let status = lifecycle.wait_for_ready().unwrap();
    assert_eq!(status.device.state, FunctionFsDeviceState::Ep0Ready);
    assert!(status.udc_bind_ready());
    assert!(!status.running());

    let status = lifecycle.bind().unwrap();
    assert_eq!(status.device.state, FunctionFsDeviceState::Ep0Ready);
    assert!(status.device.udc_bound);
    assert!(!status.running(), "a UDC bind is not endpoint traffic");
    assert!(!status.udc_bind_ready());
    assert!(status.udc_unbind_ready());

    let status = lifecycle.unbind().unwrap();
    assert_eq!(status.device.state, FunctionFsDeviceState::Unbound);
    assert!(!status.device.udc_bound);
    assert!(!status.running());

    lifecycle.cleanup().unwrap();
    assert_eq!(
        lifecycle.device_status().state,
        FunctionFsDeviceState::Unconfigured
    );
}

#[test]
fn mounter_name_is_diagnostic_only() {
    let fake = ozma_usb::FakeFunctionFsMounter::new();
    assert_eq!(FunctionFsMounter::name(&fake), "fake");
}

#[test]
#[ignore = "requires a real UDC/ConfigFS FunctionFS mount; set OZMA_FUNCTIONFS_UDC and OZMA_FUNCTIONFS_MOUNT"]
fn real_udc_binds_after_readiness_and_unbinds() {
    let Some(udc) = std::env::var_os("OZMA_FUNCTIONFS_UDC") else {
        eprintln!("OZMA_FUNCTIONFS_UDC is unset; skipping");
        return;
    };
    let Some(mountpoint) = std::env::var_os("OZMA_FUNCTIONFS_MOUNT") else {
        eprintln!("OZMA_FUNCTIONFS_MOUNT is unset; skipping");
        return;
    };
    let configfs_root = std::env::var_os("OZMA_USB_CONFIGFS_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/sys/kernel/config/usb_gadget"));
    let marker = std::path::PathBuf::from(
        std::env::var_os("OZMA_FUNCTIONFS_READY_MARKER")
            .expect("OZMA_FUNCTIONFS_READY_MARKER must name the daemon readiness marker"),
    );
    let program = std::path::PathBuf::from(
        std::env::var_os("OZMA_FUNCTIONFS_DAEMON")
            .expect("OZMA_FUNCTIONFS_DAEMON must name the running userspace daemon"),
    );
    let function = vendor_config()
        .with_daemon(ozma_usb::FunctionFsDaemonSpec::new(program).with_ready_marker(marker));
    let config = FunctionFsLifecycleConfig::new(
        "ozma-usb-state-test",
        configfs_root,
        udc.to_string_lossy(),
        function,
    )
    .with_mountpoint(std::path::PathBuf::from(mountpoint))
    .with_readiness(10_000, 100);
    let mut lifecycle = FunctionFsLifecycle::new(config)
        .unwrap()
        .with_mounter(ozma_usb::CommandFunctionFsMounter::new());

    lifecycle.prepare().unwrap();
    lifecycle.mount().unwrap();
    lifecycle.write_descriptors().unwrap();
    let status = lifecycle.wait_for_ready().unwrap();
    assert!(status.udc_bind_ready());
    let status = lifecycle.bind().unwrap();
    assert!(status.device.udc_bound);
    assert!(
        !status.running(),
        "the ConfigFS helper never opens data endpoints"
    );
    lifecycle.unbind().unwrap();
    lifecycle.cleanup().unwrap();
}
