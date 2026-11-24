// SPDX-License-Identifier: AGPL-3.0-only
//! Ports of the `virsh` fallback behavior in `softnode/libvirt_power.py`.
//!
//! No test touches real libvirt or QEMU. The backend is driven through the
//! public `CommandRunner` seam.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ozma_power::{
    CommandOutput, CommandRunner, LibvirtPower, PowerBackend, PowerError, TokioCommandRunner,
    DEFAULT_LIBVIRT_URI,
};

#[derive(Debug, Clone)]
enum Reply {
    Output(String),
    Failure { status: i32, stderr: String },
    Missing,
}

fn output(stdout: &str) -> Reply {
    Reply::Output(stdout.to_string())
}

fn failure(stderr: &str) -> Reply {
    Reply::Failure {
        status: 1,
        stderr: stderr.to_string(),
    }
}

#[derive(Debug, Default)]
struct FakeState {
    replies: VecDeque<Reply>,
    calls: Vec<Vec<String>>,
}

/// A `CommandRunner` that records argv and returns queued replies.
#[derive(Debug, Clone)]
struct FakeRunner {
    state: Arc<Mutex<FakeState>>,
}

impl FakeRunner {
    fn new(replies: Vec<Reply>) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeState {
                replies: replies.into(),
                calls: Vec::new(),
            })),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.state.lock().unwrap().calls.clone()
    }
}

#[async_trait]
impl CommandRunner for FakeRunner {
    async fn run(&self, argv: &[String]) -> Result<CommandOutput, PowerError> {
        let reply = {
            let mut state = self.state.lock().unwrap();
            state.calls.push(argv.to_vec());
            state
                .replies
                .pop_front()
                .expect("unexpected command: no fake reply queued")
        };

        match reply {
            Reply::Output(stdout) => Ok(CommandOutput::success(stdout)),
            Reply::Failure { status, stderr } => Err(PowerError::CommandFailed {
                command: argv.join(" "),
                status,
                stderr,
            }),
            Reply::Missing => Err(PowerError::MissingCommand {
                command: argv.first().cloned().unwrap_or_default(),
                remediation: "test fake".to_string(),
            }),
        }
    }
}

async fn connected(replies: Vec<Reply>) -> (LibvirtPower, FakeRunner) {
    let fake = FakeRunner::new(replies);
    let mut power = LibvirtPower::new("test-vm").with_runner(Box::new(fake.clone()));
    power.connect().await.unwrap();
    (power, fake)
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
}

// ---------------------------------------------------------------------------
// Status parsing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn query_status_parses_every_stable_state() {
    let cases = [
        ("running\n", "running"),
        ("blocked", "blocked"),
        ("paused", "paused"),
        ("in shutdown", "shutdown"),
        ("shutdown", "shutdown"),
        ("shut off", "shutoff"),
        ("shutoff", "shutoff"),
        ("crashed", "crashed"),
        ("pmsuspended", "pmsuspended"),
        ("idle", "unknown"),
        ("", "unknown"),
    ];

    for (raw, expected) in cases {
        let (mut power, fake) = connected(vec![output("running"), output(raw)]).await;
        let state = power.query_status().await.unwrap();

        assert_eq!(state.status, expected, "raw output was {raw:?}");
        assert_eq!(state.is_running(), expected == "running");
        assert_eq!(
            fake.calls(),
            vec![
                argv(&["virsh", "domstate", "test-vm"]),
                argv(&["virsh", "domstate", "test-vm"]),
            ]
        );
    }
}

#[tokio::test]
async fn query_status_requires_connect() {
    let fake = FakeRunner::new(Vec::new());
    let mut power = LibvirtPower::new("test-vm").with_runner(Box::new(fake.clone()));

    assert!(matches!(
        power.query_status().await.unwrap_err(),
        PowerError::NotConnected
    ));
    assert!(fake.calls().is_empty());
}

// ---------------------------------------------------------------------------
// cont: resume, then start fallback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cont_resumes_paused_vm_without_starting() {
    let (mut power, fake) = connected(vec![output("running"), output("")]).await;

    power.cont().await.unwrap();

    assert_eq!(
        fake.calls(),
        vec![
            argv(&["virsh", "domstate", "test-vm"]),
            argv(&["virsh", "resume", "test-vm"]),
        ]
    );
}

#[tokio::test]
async fn cont_falls_back_to_start_when_resume_fails() {
    let (mut power, fake) = connected(vec![
        output("running"),
        failure("domain is not paused"),
        output(""),
    ])
    .await;

    power.cont().await.unwrap();

    assert_eq!(
        fake.calls(),
        vec![
            argv(&["virsh", "domstate", "test-vm"]),
            argv(&["virsh", "resume", "test-vm"]),
            argv(&["virsh", "start", "test-vm"]),
        ]
    );
}

#[tokio::test]
async fn cont_reports_start_error_when_both_attempts_fail() {
    let (mut power, fake) = connected(vec![
        output("running"),
        failure("not paused"),
        failure("cannot start"),
    ])
    .await;

    let error = power.cont().await.unwrap_err();
    match error {
        PowerError::CommandFailed {
            command,
            status,
            stderr,
        } => {
            assert_eq!(command, "virsh start test-vm");
            assert_eq!(status, 1);
            assert_eq!(stderr, "cannot start");
        }
        other => panic!("expected CommandFailed, got {other:?}"),
    }
    assert_eq!(fake.calls().len(), 3);
}

#[tokio::test]
async fn cont_does_not_start_when_virsh_is_missing() {
    let (mut power, fake) = connected(vec![output("running"), Reply::Missing]).await;

    let error = power.cont().await.unwrap_err();
    assert!(matches!(error, PowerError::MissingCommand { .. }));
    assert_eq!(
        fake.calls(),
        vec![
            argv(&["virsh", "domstate", "test-vm"]),
            argv(&["virsh", "resume", "test-vm"]),
        ],
        "the start fallback must not run when virsh itself is missing"
    );
}

// ---------------------------------------------------------------------------
// shutdown / reset / destroy argv
// ---------------------------------------------------------------------------

#[tokio::test]
async fn power_actions_use_exact_virsh_argv() {
    let (mut power, fake) =
        connected(vec![output("running"), output(""), output(""), output("")]).await;

    power.system_powerdown().await.unwrap();
    power.system_reset().await.unwrap();
    power.stop().await.unwrap();

    assert_eq!(
        fake.calls(),
        vec![
            argv(&["virsh", "domstate", "test-vm"]),
            argv(&["virsh", "shutdown", "test-vm"]),
            argv(&["virsh", "reset", "test-vm"]),
            argv(&["virsh", "destroy", "test-vm"]),
        ]
    );
}

#[tokio::test]
async fn pause_and_resume_use_virsh_suspend_and_resume() {
    let (mut power, fake) = connected(vec![output("running"), output(""), output("")]).await;

    power.pause().await.unwrap();
    power.resume().await.unwrap();

    assert_eq!(
        fake.calls(),
        vec![
            argv(&["virsh", "domstate", "test-vm"]),
            argv(&["virsh", "suspend", "test-vm"]),
            argv(&["virsh", "resume", "test-vm"]),
        ],
        "pause must use virsh suspend; resume must not fall back to start"
    );
}

#[tokio::test]
async fn non_zero_exit_is_a_typed_command_failure() {
    let (mut power, _fake) = connected(vec![
        output("running"),
        Reply::Failure {
            status: 2,
            stderr: "boom\n".to_string(),
        },
    ])
    .await;

    let error = power.system_powerdown().await.unwrap_err();
    match &error {
        PowerError::CommandFailed {
            command,
            status,
            stderr,
        } => {
            assert_eq!(command, "virsh shutdown test-vm");
            assert_eq!(*status, 2);
            assert_eq!(stderr, "boom\n");
        }
        other => panic!("expected CommandFailed, got {other:?}"),
    }
    let rendered = error.to_string();
    assert!(rendered.contains("status 2"));
    assert!(rendered.contains("boom"));
}

#[tokio::test]
async fn missing_binary_is_a_typed_error_with_remediation() {
    let fake = FakeRunner::new(vec![Reply::Missing]);
    let mut power = LibvirtPower::new("test-vm").with_runner(Box::new(fake));

    let error = power.connect().await.unwrap_err();
    match &error {
        PowerError::MissingCommand { command, .. } => {
            assert_eq!(command, "virsh");
        }
        other => panic!("expected MissingCommand, got {other:?}"),
    }
    let rendered = error.to_string();
    assert!(rendered.contains("executable not found"));
}

// ---------------------------------------------------------------------------
// Connection gating
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_operation_requires_connect() {
    let fake = FakeRunner::new(Vec::new());

    let mut power = LibvirtPower::new("test-vm").with_runner(Box::new(fake.clone()));
    assert!(matches!(
        power.query_status().await.unwrap_err(),
        PowerError::NotConnected
    ));
    assert!(matches!(
        power.cont().await.unwrap_err(),
        PowerError::NotConnected
    ));
    assert!(matches!(
        power.system_powerdown().await.unwrap_err(),
        PowerError::NotConnected
    ));
    assert!(matches!(
        power.system_reset().await.unwrap_err(),
        PowerError::NotConnected
    ));
    assert!(matches!(
        power.stop().await.unwrap_err(),
        PowerError::NotConnected
    ));
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn disconnect_requires_a_new_probe() {
    let (mut power, fake) =
        connected(vec![output("running"), output("running"), output("paused")]).await;

    assert!(power.is_connected());
    power.disconnect();
    assert!(!power.is_connected());
    assert!(matches!(
        power.query_status().await.unwrap_err(),
        PowerError::NotConnected
    ));

    power.connect().await.unwrap();
    assert_eq!(power.query_status().await.unwrap().status, "paused");
    assert_eq!(fake.calls().len(), 3);
}

// ---------------------------------------------------------------------------
// Configuration validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalid_domains_are_rejected_before_running_virsh() {
    let cases = ["", "   ", " leading", "trailing ", "-option", "line\nbreak"];

    for domain in cases {
        let fake = FakeRunner::new(Vec::new());
        let mut power = LibvirtPower::new(domain).with_runner(Box::new(fake.clone()));

        let error = power.connect().await.unwrap_err();
        match error {
            PowerError::InvalidConfig { field, .. } => assert_eq!(field, "domain"),
            other => panic!("expected InvalidConfig for {domain:?}, got {other:?}"),
        }
        assert!(fake.calls().is_empty(), "virsh ran for {domain:?}");
    }
}

#[tokio::test]
async fn invalid_uris_are_rejected_before_running_virsh() {
    let cases = [
        "",
        "   ",
        "qemu:///system extra",
        "not-a-uri",
        ":///system",
        "qemu://",
    ];

    for uri in cases {
        let fake = FakeRunner::new(Vec::new());
        let mut power = LibvirtPower::new("test-vm")
            .with_uri(uri)
            .with_runner(Box::new(fake.clone()));

        let error = power.connect().await.unwrap_err();
        match error {
            PowerError::InvalidConfig { field, .. } => assert_eq!(field, "uri"),
            other => panic!("expected InvalidConfig for {uri:?}, got {other:?}"),
        }
        assert!(fake.calls().is_empty(), "virsh ran for {uri:?}");
    }
}

#[tokio::test]
async fn domain_with_a_space_stays_one_argv_element() {
    let fake = FakeRunner::new(vec![output("running")]);
    let mut power = LibvirtPower::new("my vm").with_runner(Box::new(fake.clone()));

    power.connect().await.unwrap();

    let calls = fake.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0], argv(&["virsh", "domstate", "my vm"]));
    assert_eq!(
        calls[0].len(),
        3,
        "the domain must not be split on whitespace"
    );
}

#[tokio::test]
async fn defaults_match_the_python_backend() {
    let fake = FakeRunner::new(vec![output("running")]);
    let mut power = LibvirtPower::new("test-vm").with_runner(Box::new(fake));

    assert_eq!(power.domain(), "test-vm");
    assert_eq!(power.uri(), DEFAULT_LIBVIRT_URI);
    assert!(!power.is_connected());
    power.connect().await.unwrap();
    assert!(power.is_connected());
}

// ---------------------------------------------------------------------------
// Real tokio::process runner
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tokio_runner_maps_a_missing_binary() {
    let runner = TokioCommandRunner;
    let argv = vec!["ozma-power-no-such-binary-7c1f".to_string()];

    let error = runner.run(&argv).await.unwrap_err();
    match &error {
        PowerError::MissingCommand {
            command,
            remediation,
        } => {
            assert_eq!(command, "ozma-power-no-such-binary-7c1f");
            assert!(remediation.contains("libvirt-client"));
        }
        other => panic!("expected MissingCommand, got {other:?}"),
    }
}

#[tokio::test]
async fn tokio_runner_captures_non_zero_exit_and_stderr() {
    // Test-only use of `sh`; production commands are always argv vectors.
    let argv = ["sh", "-c", "printf boom >&2; exit 3"]
        .iter()
        .map(|part| (*part).to_string())
        .collect::<Vec<_>>();

    let error = TokioCommandRunner.run(&argv).await.unwrap_err();
    match error {
        PowerError::CommandFailed {
            command,
            status,
            stderr,
        } => {
            assert!(command.starts_with("sh -c "));
            assert_eq!(status, 3);
            assert_eq!(stderr, "boom");
        }
        other => panic!("expected CommandFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn tokio_runner_captures_stdout() {
    let argv = ["sh", "-c", "printf 'running\\n'"]
        .iter()
        .map(|part| (*part).to_string())
        .collect::<Vec<_>>();

    let output = TokioCommandRunner.run(&argv).await.unwrap();
    assert_eq!(output.status, 0);
    assert_eq!(output.stdout, "running\n");
    assert_eq!(output.stderr, "");
}
