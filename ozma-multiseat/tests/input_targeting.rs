// SPDX-License-Identifier: AGPL-3.0-only
//!
//! `xinput` targeting logic against a scripted command runner. No X server,
//! `xinput` process, evdev node, or uinput device is touched.

use std::path::PathBuf;

use ozma_multiseat::{
    CommandOutput, FakeCommandRunner, InputTargetingMode, ProcessSeatInputTargeter,
    SeatInputTargetRequest, SeatInputTargeter, SeatInputTargeting,
};

const LIST: &str = "\
    Virtual core pointer                          id=2    [master pointer  (3)]
    Virtual core keyboard                         id=3    [master keyboard (2)]
    ozma-seat-host-seat-0-kbd-event0              id=10   [slave  keyboard (3)]
    ozma-seat-host-seat-0-mouse-event1            id=11   [slave  pointer  (2)]
    ozma-seat-host-seat-1-kbd-event2              id=20   [slave  keyboard (3)]
    ozma-seat-host-seat-1-mouse-event3            id=21   [slave  pointer  (2)]
";

fn request(devices: &[&str]) -> SeatInputTargetRequest {
    SeatInputTargetRequest {
        seat_id: "host-seat-0".to_owned(),
        display: ":90".to_owned(),
        xauthority: Some(PathBuf::from("/tmp/ozma-seat-0.Xauthority")),
        devices: devices.iter().map(|device| (*device).to_owned()).collect(),
    }
}

fn own() -> Vec<&'static str> {
    vec![
        "ozma-seat-host-seat-0-kbd-event0",
        "ozma-seat-host-seat-0-mouse-event1",
    ]
}

#[test]
fn disables_only_other_seat_helper_devices_then_enables_own() {
    let runner = FakeCommandRunner::new()
        .respond_stdout("xinput", &["list", "--short"], LIST)
        .respond("xinput", &["disable", "20"], CommandOutput::success())
        .respond("xinput", &["disable", "21"], CommandOutput::success())
        .respond("xinput", &["enable", "10"], CommandOutput::success())
        .respond("xinput", &["enable", "11"], CommandOutput::success());
    let mut targeter = ProcessSeatInputTargeter::with_runner(Box::new(runner.clone()));

    let targeting = targeter.apply(&request(&own())).expect("target display");

    assert_eq!(
        targeting,
        SeatInputTargeting {
            seat_id: "host-seat-0".to_owned(),
            display: ":90".to_owned(),
            enabled: own().iter().map(|name| (*name).to_owned()).collect(),
            disabled: vec![
                "ozma-seat-host-seat-1-kbd-event2".to_owned(),
                "ozma-seat-host-seat-1-mouse-event3".to_owned(),
            ],
        }
    );

    let commands = runner.commands();
    let args: Vec<Vec<String>> = commands
        .iter()
        .map(|command| command.args.clone())
        .collect();
    assert_eq!(
        args,
        vec![
            vec!["list".to_owned(), "--short".to_owned()],
            vec!["disable".to_owned(), "20".to_owned()],
            vec!["disable".to_owned(), "21".to_owned()],
            vec!["enable".to_owned(), "10".to_owned()],
            vec!["enable".to_owned(), "11".to_owned()],
        ]
    );
    assert!(commands
        .iter()
        .all(|command| command.env.get("DISPLAY").map(String::as_str) == Some(":90")));
    assert!(commands.iter().all(|command| {
        command.env.get("XAUTHORITY").map(String::as_str) == Some("/tmp/ozma-seat-0.Xauthority")
    }));
    assert_eq!(targeter.status_for("host-seat-0"), Some(targeting));
}

#[test]
fn request_with_only_own_devices_never_disables_a_core_device() {
    let list = "\
        Virtual core pointer     id=2    [master pointer  (3)]
        Virtual core keyboard    id=3    [master keyboard (2)]
        ozma-seat-host-seat-0-kbd-event0 id=10 [slave keyboard (3)]
    ";
    let runner = FakeCommandRunner::new()
        .respond_stdout("xinput", &["list", "--short"], list)
        .respond("xinput", &["enable", "10"], CommandOutput::success());
    let mut targeter = ProcessSeatInputTargeter::with_runner(Box::new(runner.clone()));

    let targeting = targeter
        .apply(&request(&["ozma-seat-host-seat-0-kbd-event0"]))
        .expect("target display");
    assert!(targeting.disabled.is_empty());
    assert_eq!(
        targeting.enabled,
        vec!["ozma-seat-host-seat-0-kbd-event0".to_owned()]
    );
    let commands = runner.commands();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[1].args, vec!["enable".to_owned(), "10".to_owned()]);
}

#[test]
fn missing_own_uinput_device_is_typed_and_changes_nothing() {
    let runner = FakeCommandRunner::new().respond_stdout("xinput", &["list", "--short"], LIST);
    let mut targeter = ProcessSeatInputTargeter::with_runner(Box::new(runner.clone()));

    let error = targeter
        .apply(&request(&[
            "ozma-seat-host-seat-0-kbd-event0",
            "ozma-seat-host-seat-0-mouse-event9",
        ]))
        .expect_err("missing own device");
    assert!(error.to_string().contains("not visible"));
    assert_eq!(runner.commands().len(), 1, "only the list may run");
    assert!(!targeter.status_for("host-seat-0").is_some());
}

#[test]
fn release_reenables_the_other_seat_devices_and_is_idempotent() {
    let runner = FakeCommandRunner::new()
        .respond_stdout("xinput", &["list", "--short"], LIST)
        .respond("xinput", &["disable", "20"], CommandOutput::success())
        .respond("xinput", &["disable", "21"], CommandOutput::success())
        .respond("xinput", &["enable", "10"], CommandOutput::success())
        .respond("xinput", &["enable", "11"], CommandOutput::success())
        .respond("xinput", &["enable", "20"], CommandOutput::success())
        .respond("xinput", &["enable", "21"], CommandOutput::success());
    let mut targeter = ProcessSeatInputTargeter::with_runner(Box::new(runner.clone()));

    targeter.apply(&request(&own())).expect("target");
    let released = targeter.release("host-seat-0").expect("release");
    assert_eq!(
        released.reenabled,
        vec![
            "ozma-seat-host-seat-1-kbd-event2".to_owned(),
            "ozma-seat-host-seat-1-mouse-event3".to_owned(),
        ]
    );
    assert!(targeter.status_for("host-seat-0").is_none());

    let commands = runner.commands();
    assert_eq!(
        commands[commands.len() - 2].args,
        vec!["enable".to_owned(), "20".to_owned()]
    );
    assert_eq!(
        commands[commands.len() - 1].args,
        vec!["enable".to_owned(), "21".to_owned()]
    );

    // A second release has no assignment and runs no command.
    let before = runner.run_count();
    let again = targeter.release("host-seat-0").expect("idempotent");
    assert!(again.reenabled.is_empty());
    assert_eq!(runner.run_count(), before);
}

#[test]
fn disable_failure_aborts_without_claiming_targeting() {
    let runner = FakeCommandRunner::new()
        .respond_stdout("xinput", &["list", "--short"], LIST)
        .respond_failure("xinput", &["disable", "20"], 1, "cannot disable");
    let mut targeter = ProcessSeatInputTargeter::with_runner(Box::new(runner.clone()));

    let error = targeter.apply(&request(&own())).expect_err("disable fails");
    assert!(error.to_string().contains("cannot disable"));
    assert!(targeter.status_for("host-seat-0").is_none());
    assert_eq!(runner.commands().len(), 2);
}

#[test]
fn enable_failure_rolls_back_disabled_foreign_devices() {
    let runner = FakeCommandRunner::new()
        .respond_stdout("xinput", &["list", "--short"], LIST)
        .respond("xinput", &["disable", "20"], CommandOutput::success())
        .respond("xinput", &["disable", "21"], CommandOutput::success())
        .respond_failure("xinput", &["enable", "10"], 1, "cannot enable")
        .respond("xinput", &["enable", "21"], CommandOutput::success())
        .respond("xinput", &["enable", "20"], CommandOutput::success());
    let mut targeter = ProcessSeatInputTargeter::with_runner(Box::new(runner.clone()));

    let error = targeter.apply(&request(&own())).expect_err("enable fails");
    assert!(error.to_string().contains("cannot enable"));
    assert!(targeter.status_for("host-seat-0").is_none());
    let commands = runner.commands();
    assert_eq!(
        commands[commands.len() - 1].args,
        vec!["enable".to_owned(), "20".to_owned()]
    );
    assert_eq!(
        commands[commands.len() - 2].args,
        vec!["enable".to_owned(), "21".to_owned()]
    );
}

#[test]
fn mode_parsing_is_explicit() {
    assert_eq!(
        "off".parse::<InputTargetingMode>().unwrap(),
        InputTargetingMode::Off
    );
    assert_eq!(
        "warn".parse::<InputTargetingMode>().unwrap(),
        InputTargetingMode::Warn
    );
    assert_eq!(
        "strict".parse::<InputTargetingMode>().unwrap(),
        InputTargetingMode::Strict
    );
    assert!("loud".parse::<InputTargetingMode>().is_err());
}
