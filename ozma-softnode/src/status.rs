//! Aggregated multi-VM supervisor status, metrics, and HTTP surface.
//!
//! The supervisor launches one `ozma-node` per VM but historically had no
//! aggregate view: an operator had to read logs or poll each child's HTTP port.
//! This module defines the status document, a bounded Prometheus text surface,
//! and a small read-only HTTP server (`/health`, `/status`, `/metrics`,
//! `/api/v1/connection`) served on `--status-port`. Port `0` disables it.
//!
//! The documents are deliberately honest and bounded:
//!
//! * only VMs the supervisor currently tracks (plus names excluded by
//!   `--exclude`) appear, so cardinality is bounded by the hypervisor's VM set;
//! * no sockets are opened, no child is probed, and no QEMU/virsh command runs
//!   while answering a request (optional child polling runs in its own task);
//! * QMP socket paths and policy values stay in `/status`; metrics only carry
//!   VM names and numeric state so a scrape does not leak host paths or
//!   secrets.

use std::sync::Arc;

use axum::{
    extract::State,
    http::header,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::manager::VirtualNodeManager;
use crate::meeting::MeetingDetectStatus;
use crate::supervisor::NodePorts;

/// Profile name reported by the supervisor's connection document.
pub const SUPERVISOR_PROFILE: &str = "hypervisor";
/// Stable node id for the supervisor process. Per-VM children carry their VM
/// name; the supervisor is one process for the whole hypervisor.
pub const SUPERVISOR_NODE_ID: &str = "ozma-virtual-node";

/// Aggregated supervisor status document served by `GET /status`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SupervisorStatus {
    pub ok: bool,
    /// Crate version, so a controller can detect an old supervisor.
    pub version: String,
    /// Always [`SUPERVISOR_PROFILE`] (`hypervisor`).
    pub profile: String,
    /// Always [`SUPERVISOR_NODE_ID`]; the supervisor is one process for the
    /// whole hypervisor.
    pub node_id: String,
    /// Port this status server is bound to (`0` when the listener is disabled).
    pub status_port: u16,
    /// Child `/api/v1/connection` poll interval in milliseconds; `0` when child
    /// aggregation is disabled.
    #[serde(default)]
    pub child_poll_interval_ms: u64,
    /// Per-child poll timeout in milliseconds.
    #[serde(default)]
    pub child_poll_timeout_ms: u64,
    /// Aggregate counts and the next free per-VM ports.
    pub supervisor: SupervisorTotals,
    /// One entry per tracked VM, plus excluded VMs (state `excluded`).
    pub vms: Vec<VmStatus>,
    /// Opt-in meeting-detection bridge report. Defaults to `disabled`, so a
    /// controller can distinguish an old/inactive supervisor from a live call.
    #[serde(default)]
    pub meeting: MeetingDetectStatus,
}

impl SupervisorStatus {
    /// Build a status document and derive its totals from the VM entries.
    pub fn new(discovered: usize, next_ports: Option<NodePorts>, vms: Vec<VmStatus>) -> Self {
        let mut supervisor = SupervisorTotals {
            discovered,
            tracked: 0,
            running: 0,
            failed: 0,
            skipped: 0,
            excluded: 0,
            next_hid_port: next_ports.map(|ports| ports.hid_udp),
            next_http_port: next_ports.map(|ports| ports.http),
        };
        for vm in &vms {
            supervisor.observe(vm);
        }
        Self {
            ok: true,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            profile: SUPERVISOR_PROFILE.to_owned(),
            node_id: SUPERVISOR_NODE_ID.to_owned(),
            status_port: 0,
            child_poll_interval_ms: 0,
            child_poll_timeout_ms: 0,
            supervisor,
            vms,
            meeting: MeetingDetectStatus::default(),
        }
    }
}

/// Supervisor totals exposed in `/status`, `/health`, and
/// `/api/v1/connection`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorTotals {
    /// VMs in the last discovery snapshot, including excluded ones.
    pub discovered: usize,
    /// VMs the supervisor tracks (discovered minus excluded).
    pub tracked: usize,
    /// VMs with a live child process.
    pub running: usize,
    /// VMs whose child failed and is waiting to restart, or whose provisioning
    /// failed.
    pub failed: usize,
    /// Tracked VMs with no child: no QMP socket, awaiting restart, or
    /// discovery-only mode.
    pub skipped: usize,
    /// VMs filtered out by `--exclude`.
    pub excluded: usize,
    /// HID UDP port the next new VM would receive, when one is available.
    pub next_hid_port: Option<u16>,
    /// HTTP port the next new VM would receive, when one is available.
    pub next_http_port: Option<u16>,
}

impl SupervisorTotals {
    fn observe(&mut self, vm: &VmStatus) {
        if vm.excluded {
            self.excluded += 1;
            return;
        }
        self.tracked += 1;
        if vm.spawned {
            self.running += 1;
        }
        match vm.state.as_str() {
            "backoff" | "failed" => self.failed += 1,
            "running" | "starting" => {}
            _ if !vm.spawned => self.skipped += 1,
            _ => {}
        }
    }
}

/// One VM's supervisor-visible state.
///
/// `state` is one of:
///
/// * `running` — child process alive;
/// * `starting` — spawn in flight;
/// * `backoff` — child exited or failed to spawn; restart is scheduled;
/// * `stopped` — tracked node with no child and no scheduled restart;
/// * `skipped` — no QMP socket and auto-manage is on;
/// * `needs-restart` — XML defined but the VM must restart before a socket
///   exists;
/// * `awaiting-socket` — a configured QMP path does not exist yet;
/// * `failed` — provisioning failed;
/// * `argv-error` — a configured child option was rejected (for example an
///   unsupported flag under `--reject-unsupported-node-flags`);
/// * `discovery-only` — `--no-auto-manage`;
/// * `excluded` — filtered by `--exclude`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmStatus {
    pub name: String,
    pub state: String,
    /// Control QMP socket the child uses, or the discovered path. Empty when
    /// no usable socket exists.
    pub qmp_socket: String,
    /// Allocated HID UDP port; `None` when no node was launched.
    pub hid_udp_port: Option<u16>,
    /// Allocated HTTP port; `None` when no node was launched.
    pub http_port: Option<u16>,
    /// Child process id, when the launcher reported one.
    pub child_pid: Option<u32>,
    /// Whether a child handle currently exists.
    pub spawned: bool,
    /// Whether this VM has no child (skipped, excluded, awaiting, or failed).
    pub skipped: bool,
    /// Whether the VM must restart before a node can start.
    pub needs_restart: bool,
    /// Whether `--exclude` filtered this VM out.
    pub excluded: bool,
    /// Successful (re)starts after the initial spawn.
    pub restart_count: u32,
    /// Milliseconds until the next restart, when one is scheduled.
    pub restart_in_ms: Option<u64>,
    /// Last child exit, for example `exit code 1`.
    pub last_exit: Option<String>,
    /// Most recent spawn/exit/provisioning error.
    pub last_error: Option<String>,
    /// `ready` when this supervisor created evdev devices, else
    /// `not-configured`.
    pub evdev_status: String,
    pub evdev_keyboard: Option<String>,
    pub evdev_mouse: Option<String>,
    /// Per-VM audio sink passed to the child as `--audio-sink`.
    pub audio_sink: Option<String>,
    /// Exact child argv the supervisor would spawn, with secret values
    /// replaced by `<redacted>`. Empty until a node spec has been built.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_argv: Vec<String>,
    /// Configured child options that were not forwarded, for example a flag
    /// the target `ozma-node` build does not advertise or a GPU-passthrough VM
    /// with no Looking Glass path. Empty when every configured option was
    /// forwarded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv_warnings: Vec<String>,
    /// `provisioned`, `pending`, `retrying`, `failed`, or `disabled`.
    pub agent_status: String,
    /// Consecutive failed in-guest agent checks.
    pub agent_checks: u32,
    /// Cached summary of the child's own `/api/v1/connection` document.
    /// `None` when child polling is disabled or the child has never been polled.
    #[serde(default)]
    pub child: Option<ChildStatus>,
}

impl VmStatus {
    /// A minimal placeholder for an excluded VM, which is never tracked.
    pub fn excluded(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            state: "excluded".to_owned(),
            qmp_socket: String::new(),
            hid_udp_port: None,
            http_port: None,
            child_pid: None,
            spawned: false,
            skipped: true,
            needs_restart: false,
            excluded: true,
            restart_count: 0,
            restart_in_ms: None,
            last_exit: None,
            last_error: None,
            evdev_status: "not-configured".to_owned(),
            evdev_keyboard: None,
            evdev_mouse: None,
            audio_sink: None,
            child_argv: Vec::new(),
            argv_warnings: Vec::new(),
            agent_status: "disabled".to_owned(),
            agent_checks: 0,
            child: None,
        }
    }
}

/// One running child's cached `/api/v1/connection` summary.
///
/// The supervisor polls only the handful of fields needed for a multi-VM view;
/// it does not copy the full child document. Ages and `stale` are derived at
/// status read time, so a cached entry cannot claim to be fresher than it is.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildStatus {
    /// Whether the most recent poll reached the child and it reported `ok:true`.
    pub reachable: bool,
    /// Child node id reported by `/api/v1/connection`, when present.
    pub node_id: Option<String>,
    /// Child profile (`vm`), when reported.
    pub profile: Option<String>,
    /// Child HTTP API port, when reported.
    pub api_port: Option<u16>,
    /// HLS state (`running`, `not-configured`, or `failed`).
    pub hls: Option<String>,
    /// WebRTC state (`running`, `not-configured`, or `failed`).
    pub webrtc: Option<String>,
    /// Audio routing state (`running`, `not-configured`, or `failed`).
    pub audio: Option<String>,
    /// Power backend state, when reported.
    pub power: Option<ChildPowerStatus>,
    /// Input sink state, when reported.
    pub input: Option<ChildInputStatus>,
    /// Age of the most recent poll attempt, successful or not.
    pub last_poll_age_ms: Option<u64>,
    /// Age of the most recent successful poll.
    pub last_seen_age_ms: Option<u64>,
    /// Wall-clock time (Unix milliseconds) of the most recent successful poll.
    pub last_seen_unix_ms: Option<u64>,
    /// True when no successful poll is recent enough for the configured
    /// interval and timeout. A child that was never polled is stale.
    pub stale: bool,
    /// Most recent transport/HTTP/parse error. Never contains a token.
    pub last_error: Option<String>,
}

/// Child power backend summary. The child reports either a status string or an
/// object with `status`/`available`; both are normalised here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildPowerStatus {
    pub state: String,
    pub available: bool,
}

/// Child input sink summary.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildInputStatus {
    /// Controller-facing sink kind (`uinput`, `qmp`, `hid-gadget`, ...).
    pub kind: String,
    /// Whether a shared sink is attached.
    pub available: bool,
    /// Whether events are really injected.
    pub injecting: bool,
}

/// `GET /api/v1/connection`-like document for the supervisor.
///
/// Field names mirror the per-VM node's connection response closely enough for
/// a controller to identify the process, but `profile` is `hypervisor` and
/// `supervisor` carries aggregate counts instead of per-node HID/codec state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionResponse {
    pub ok: bool,
    pub node_id: String,
    pub profile: String,
    pub version: String,
    pub api_port: u16,
    /// Always `0`: the supervisor opens no QUIC listener of its own.
    pub quic_port: u16,
    /// Always `0`: HID UDP is per child, not the supervisor.
    pub hid_udp_port: u16,
    /// Always `supervisor`; the supervisor is not an input sink.
    pub sink_kind: String,
    pub supervisor: SupervisorTotals,
}

/// `GET /health` document. Kept small so a controller can poll it cheaply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub version: String,
    pub profile: String,
    pub discovered: usize,
    pub running: usize,
    pub failed: usize,
    pub skipped: usize,
}

#[derive(Clone)]
struct StatusState {
    manager: Arc<VirtualNodeManager>,
    status_port: u16,
    meeting: watch::Receiver<MeetingDetectStatus>,
}

/// Build the read-only status router.
pub fn router(
    manager: Arc<VirtualNodeManager>,
    status_port: u16,
    meeting: watch::Receiver<MeetingDetectStatus>,
) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/metrics", get(metrics))
        .route("/api/v1/connection", get(connection))
        .with_state(StatusState {
            manager,
            status_port,
            meeting,
        })
}

/// Bind the supervisor status listener on all interfaces.
///
/// Port `0` is allowed and selects an ephemeral port, which is useful for
/// tests; the CLI rejects `0` by simply not starting the server.
pub async fn bind(port: u16) -> anyhow::Result<TcpListener> {
    TcpListener::bind(("0.0.0.0", port))
        .await
        .map_err(|error| anyhow::anyhow!("binding supervisor status port {port}: {error}"))
}

/// Serve the status API until `shutdown` becomes `true`.
pub async fn serve(
    manager: Arc<VirtualNodeManager>,
    listener: TcpListener,
    status_port: u16,
    mut shutdown: watch::Receiver<bool>,
    meeting: watch::Receiver<MeetingDetectStatus>,
) -> anyhow::Result<()> {
    let app = router(manager, status_port, meeting);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            // A dropped sender also ends the wait; the runtime is shutting
            // down in that case, so stopping the listener is correct.
            let _ = shutdown.wait_for(|stopped| *stopped).await;
        })
        .await
        .map_err(|error| anyhow::anyhow!("serving supervisor status API: {error}"))
}

async fn health(State(state): State<StatusState>) -> Json<HealthResponse> {
    let status = state.manager.status().await;
    Json(HealthResponse {
        ok: true,
        version: status.version,
        profile: status.profile,
        discovered: status.supervisor.discovered,
        running: status.supervisor.running,
        failed: status.supervisor.failed,
        skipped: status.supervisor.skipped,
    })
}

async fn status(State(state): State<StatusState>) -> Json<SupervisorStatus> {
    let mut status = state.manager.status().await;
    status.status_port = state.status_port;
    status.meeting = state.meeting.borrow().refreshed();
    Json(status)
}

async fn connection(State(state): State<StatusState>) -> Json<ConnectionResponse> {
    let status = state.manager.status().await;
    Json(ConnectionResponse {
        ok: true,
        node_id: status.node_id,
        profile: status.profile,
        version: status.version,
        api_port: state.status_port,
        quic_port: 0,
        hid_udp_port: 0,
        sink_kind: "supervisor".to_owned(),
        supervisor: status.supervisor,
    })
}

async fn metrics(State(state): State<StatusState>) -> Response {
    let status = state.manager.status().await;
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        render_metrics(&status),
    )
        .into_response()
}

/// Render the bounded Prometheus text surface.
///
/// Labels are limited to VM names and fixed `state`/`kind` values. QMP paths,
/// errors, and sink names stay out of metrics to bound cardinality and avoid
/// leaking host details to a scraper.
pub fn render_metrics(status: &SupervisorStatus) -> String {
    let totals = &status.supervisor;
    let mut out = String::new();
    out.push_str(
        "# HELP ozma_supervisor_vms Supervised VMs by aggregate state.\n\
         # TYPE ozma_supervisor_vms gauge\n",
    );
    for (state, value) in [
        ("discovered", totals.discovered),
        ("tracked", totals.tracked),
        ("running", totals.running),
        ("failed", totals.failed),
        ("skipped", totals.skipped),
        ("excluded", totals.excluded),
    ] {
        out.push_str(&format!(
            "ozma_supervisor_vms{{state=\"{}\"}} {}\n",
            escape_label(state),
            value
        ));
    }

    out.push_str(
        "# HELP ozma_supervisor_next_port Next per-VM port that would be allocated.\n\
         # TYPE ozma_supervisor_next_port gauge\n",
    );
    for (kind, value) in [
        ("hid_udp", totals.next_hid_port),
        ("http", totals.next_http_port),
    ] {
        if let Some(port) = value {
            out.push_str(&format!(
                "ozma_supervisor_next_port{{kind=\"{}\"}} {}\n",
                escape_label(kind),
                port
            ));
        }
    }

    out.push_str(
        "# HELP ozma_supervisor_vm_up Whether a child process is alive for this VM.\n\
         # TYPE ozma_supervisor_vm_up gauge\n",
    );
    for vm in &status.vms {
        out.push_str(&format!(
            "ozma_supervisor_vm_up{{vm=\"{}\"}} {}\n",
            escape_label(&vm.name),
            u8::from(vm.spawned)
        ));
    }

    out.push_str(
        "# HELP ozma_supervisor_vm_restarts_total Successful child restarts after failures.\n\
         # TYPE ozma_supervisor_vm_restarts_total counter\n",
    );
    for vm in &status.vms {
        out.push_str(&format!(
            "ozma_supervisor_vm_restarts_total{{vm=\"{}\"}} {}\n",
            escape_label(&vm.name),
            vm.restart_count
        ));
    }

    out.push_str(
        "# HELP ozma_supervisor_vm_port Port allocated to a VM's child, by kind.\n\
         # TYPE ozma_supervisor_vm_port gauge\n",
    );
    for vm in &status.vms {
        for (kind, value) in [("hid_udp", vm.hid_udp_port), ("http", vm.http_port)] {
            if let Some(port) = value {
                out.push_str(&format!(
                    "ozma_supervisor_vm_port{{vm=\"{}\",kind=\"{}\"}} {}\n",
                    escape_label(&vm.name),
                    escape_label(kind),
                    port
                ));
            }
        }
    }

    out.push_str(
        "# HELP ozma_supervisor_vm_state Current supervisor state as a labelled gauge.\n\
         # TYPE ozma_supervisor_vm_state gauge\n",
    );
    for vm in &status.vms {
        out.push_str(&format!(
            "ozma_supervisor_vm_state{{vm=\"{}\",state=\"{}\"}} 1\n",
            escape_label(&vm.name),
            escape_label(&vm.state)
        ));
    }

    // Child API aggregation is optional. Only reachability and staleness are
    // exported: child error strings and sink names stay out of metrics.
    let mut wrote_child_header = false;
    for vm in &status.vms {
        let Some(child) = &vm.child else { continue };
        if !wrote_child_header {
            out.push_str(
                "# HELP ozma_supervisor_vm_child_up Whether the last child API poll succeeded.\n\
                 # TYPE ozma_supervisor_vm_child_up gauge\n",
            );
            wrote_child_header = true;
        }
        out.push_str(&format!(
            "ozma_supervisor_vm_child_up{{vm=\"{}\"}} {}\n",
            escape_label(&vm.name),
            u8::from(child.reachable)
        ));
    }
    if wrote_child_header {
        out.push_str(
            "# HELP ozma_supervisor_vm_child_stale Whether the cached child summary is stale.\n\
             # TYPE ozma_supervisor_vm_child_stale gauge\n",
        );
        for vm in &status.vms {
            if let Some(child) = &vm.child {
                out.push_str(&format!(
                    "ozma_supervisor_vm_child_stale{{vm=\"{}\"}} {}\n",
                    escape_label(&vm.name),
                    u8::from(child.stale)
                ));
            }
        }
    }
    out
}

fn escape_label(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;
    use crate::ipc::MeetingPlatform;
    use crate::manager::ManagerOptions;
    use crate::meeting::{MeetingDetectState, MeetingReport, MeetingState};
    use crate::supervisor::testing::FakeNodeLauncher;
    use crate::vm_info::VMInfo;

    fn vm(name: &str, qmp: &str) -> VMInfo {
        let mut vm = VMInfo::new(name);
        vm.qmp_path = qmp.to_owned();
        vm
    }

    fn manager(launcher: Arc<FakeNodeLauncher>) -> VirtualNodeManager {
        VirtualNodeManager::with_launcher(
            ManagerOptions {
                auto_manage: true,
                auto_agent: false,
                node_bin: PathBuf::from("/usr/bin/ozma-node"),
                base_port: 7600,
                node_http_base: 7700,
                restart_backoff: Duration::from_millis(2),
                restart_backoff_max: Duration::from_millis(8),
                ..ManagerOptions::default()
            },
            launcher,
        )
    }

    async fn body_json(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("response body");
        serde_json::from_slice(&bytes).expect("json body")
    }

    fn meeting_channel() -> watch::Receiver<MeetingDetectStatus> {
        meeting_channel_with(MeetingDetectStatus::default())
    }

    fn meeting_channel_with(status: MeetingDetectStatus) -> watch::Receiver<MeetingDetectStatus> {
        watch::channel(status).1
    }

    fn sample_status() -> SupervisorStatus {
        SupervisorStatus::new(
            2,
            Some(NodePorts {
                hid_udp: 7610,
                http: 7710,
            }),
            vec![
                VmStatus {
                    name: "vm-a".to_owned(),
                    state: "running".to_owned(),
                    qmp_socket: "/run/ozma/a.sock".to_owned(),
                    hid_udp_port: Some(7600),
                    http_port: Some(7700),
                    child_pid: Some(42),
                    spawned: true,
                    skipped: false,
                    needs_restart: false,
                    excluded: false,
                    restart_count: 1,
                    restart_in_ms: None,
                    last_exit: Some("exit code 1".to_owned()),
                    last_error: None,
                    evdev_status: "not-configured".to_owned(),
                    evdev_keyboard: None,
                    evdev_mouse: None,
                    audio_sink: Some("ozma-vm-a".to_owned()),
                    child_argv: vec![
                        "ozma-node".to_owned(),
                        "--profile".to_owned(),
                        "vm".to_owned(),
                    ],
                    argv_warnings: Vec::new(),
                    agent_status: "pending".to_owned(),
                    agent_checks: 0,
                    child: Some(ChildStatus {
                        reachable: true,
                        node_id: Some("vm-a".to_owned()),
                        profile: Some("vm".to_owned()),
                        api_port: Some(7700),
                        hls: Some("running".to_owned()),
                        webrtc: Some("not-configured".to_owned()),
                        audio: Some("running".to_owned()),
                        power: Some(ChildPowerStatus {
                            state: "running".to_owned(),
                            available: true,
                        }),
                        input: Some(ChildInputStatus {
                            kind: "qmp".to_owned(),
                            available: true,
                            injecting: true,
                        }),
                        last_poll_age_ms: Some(250),
                        last_seen_age_ms: Some(250),
                        last_seen_unix_ms: Some(1_700_000_000_000),
                        stale: false,
                        last_error: None,
                    }),
                },
                VmStatus::excluded("infra-template"),
            ],
        )
    }

    #[test]
    fn status_json_has_the_documented_shape_and_totals() {
        let status = sample_status();
        let json = serde_json::to_value(&status).expect("serialize status");

        assert_eq!(json["ok"], true);
        assert_eq!(json["profile"], "hypervisor");
        assert_eq!(json["node_id"], SUPERVISOR_NODE_ID);
        assert!(json["version"].is_string());
        assert_eq!(json["supervisor"]["discovered"], 2);
        assert_eq!(json["supervisor"]["tracked"], 1);
        assert_eq!(json["supervisor"]["running"], 1);
        assert_eq!(json["supervisor"]["failed"], 0);
        assert_eq!(json["supervisor"]["skipped"], 0);
        assert_eq!(json["supervisor"]["excluded"], 1);
        assert_eq!(json["supervisor"]["next_hid_port"], 7610);
        assert_eq!(json["supervisor"]["next_http_port"], 7710);
        assert_eq!(json["child_poll_interval_ms"], 0);
        assert_eq!(json["child_poll_timeout_ms"], 0);
        assert_eq!(json["meeting"]["state"], "disabled");
        assert_eq!(json["meeting"]["meeting_state"], "unknown");
        assert_eq!(json["vms"].as_array().unwrap().len(), 2);

        let vm = &json["vms"][0];
        for key in [
            "name",
            "state",
            "qmp_socket",
            "hid_udp_port",
            "http_port",
            "child_pid",
            "spawned",
            "skipped",
            "needs_restart",
            "excluded",
            "restart_count",
            "restart_in_ms",
            "last_exit",
            "last_error",
            "evdev_status",
            "evdev_keyboard",
            "evdev_mouse",
            "audio_sink",
            "child_argv",
            "agent_status",
            "agent_checks",
            "child",
        ] {
            assert!(vm.get(key).is_some(), "missing /status.vms[].{key}");
        }
        assert_eq!(vm["child"]["reachable"], true);
        assert_eq!(vm["child"]["hls"], "running");
        assert_eq!(vm["child"]["input"]["kind"], "qmp");
        assert_eq!(vm["child"]["stale"], false);
        assert!(
            vm.get("argv_warnings").is_none(),
            "empty argv warnings are omitted from /status"
        );

        // Round-trip keeps the schema stable.
        let decoded: SupervisorStatus = serde_json::from_value(json).expect("deserialize status");
        assert_eq!(decoded, status);
    }

    #[test]
    fn metrics_are_bounded_and_omit_socket_paths() {
        let text = render_metrics(&sample_status());

        assert!(text.contains("ozma_supervisor_vms{state=\"discovered\"} 2"));
        assert!(text.contains("ozma_supervisor_vms{state=\"running\"} 1"));
        assert!(text.contains("ozma_supervisor_vms{state=\"excluded\"} 1"));
        assert!(text.contains("ozma_supervisor_next_port{kind=\"hid_udp\"} 7610"));
        assert!(text.contains("ozma_supervisor_vm_up{vm=\"vm-a\"} 1"));
        assert!(text.contains("ozma_supervisor_vm_restarts_total{vm=\"vm-a\"} 1"));
        assert!(text.contains("ozma_supervisor_vm_port{vm=\"vm-a\",kind=\"http\"} 7700"));
        assert!(text.contains("ozma_supervisor_vm_state{vm=\"vm-a\",state=\"running\"} 1"));
        assert!(text.contains("ozma_supervisor_vm_child_up{vm=\"vm-a\"} 1"));
        assert!(text.contains("ozma_supervisor_vm_child_stale{vm=\"vm-a\"} 0"));
        assert!(
            !text.contains("not-configured"),
            "metrics must not leak child state labels"
        );
        assert!(
            !text.contains("/run/ozma"),
            "metrics must not leak QMP paths"
        );
        assert!(
            !text.contains("ozma-vm-a"),
            "metrics must not leak sink names"
        );
    }

    #[test]
    fn starting_vms_are_not_counted_as_skipped() {
        let mut vm = sample_status().vms[0].clone();
        vm.state = "starting".to_owned();
        vm.spawned = false;
        vm.skipped = true;

        let status = SupervisorStatus::new(1, None, vec![vm]);
        assert_eq!(status.supervisor.tracked, 1);
        assert_eq!(status.supervisor.running, 0);
        assert_eq!(status.supervisor.skipped, 0);
        assert_eq!(status.supervisor.failed, 0);
    }

    #[test]
    fn metric_labels_escape_quotes_and_backslashes() {
        assert_eq!(escape_label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[tokio::test]
    async fn routes_report_health_status_metrics_and_connection() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mgr = Arc::new(manager(Arc::clone(&launcher)));
        mgr.sync_for_test(vec![vm("vm-a", "/run/ozma/a.sock")])
            .await;
        let app = router(Arc::clone(&mgr), 9555, meeting_channel());

        let health = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
        let health = body_json(health).await;
        assert_eq!(health["ok"], true);
        assert_eq!(health["profile"], "hypervisor");
        assert_eq!(health["discovered"], 1);
        assert_eq!(health["running"], 1);

        let status = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::OK);
        let status = body_json(status).await;
        assert_eq!(status["status_port"], 9555);
        assert_eq!(status["vms"][0]["name"], "vm-a");
        assert_eq!(status["vms"][0]["state"], "running");
        assert_eq!(status["vms"][0]["hid_udp_port"], 7600);
        assert_eq!(status["vms"][0]["http_port"], 7700);
        assert_eq!(status["vms"][0]["qmp_socket"], "/run/ozma/a.sock");

        let metrics = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(metrics.status(), StatusCode::OK);
        assert_eq!(
            metrics.headers()[header::CONTENT_TYPE],
            "text/plain; version=0.0.4"
        );
        let text = String::from_utf8(
            to_bytes(metrics.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(text.contains("ozma_supervisor_vm_up{vm=\"vm-a\"} 1"));

        let connection = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/connection")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(connection.status(), StatusCode::OK);
        let connection = body_json(connection).await;
        assert_eq!(connection["ok"], true);
        assert_eq!(connection["profile"], "hypervisor");
        assert_eq!(connection["api_port"], 9555);
        assert_eq!(connection["quic_port"], 0);
        assert_eq!(connection["hid_udp_port"], 0);
        assert_eq!(connection["supervisor"]["running"], 1);
    }

    #[tokio::test]
    async fn status_exposes_a_real_meeting_but_not_paths_or_attendee_addresses() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mgr = Arc::new(manager(launcher));
        mgr.sync_for_test(vec![vm("vm-a", "/run/ozma/a.sock")])
            .await;

        let meeting = MeetingDetectStatus {
            enabled: true,
            state: MeetingDetectState::Running,
            meeting_state: MeetingState::Active,
            connected: true,
            manage_child: false,
            detector_interval_secs: 5,
            poll_interval_ms: 2000,
            last_update_unix_ms: Some(1_700_000_000_000),
            last_detection_unix_ms: Some(1_700_000_000_000),
            last_detection: Some(MeetingReport {
                id: "zoom-1".to_owned(),
                platform: MeetingPlatform::Zoom,
                active: true,
                started_at_unix_ms: 1_700_000_000_000,
                duration_s: None,
                title: "Standup".to_owned(),
                organizer: "host@example.test".to_owned(),
                attendee_count: 3,
                has_calendar_metadata: true,
            }),
            meetings: vec![MeetingReport {
                id: "zoom-1".to_owned(),
                platform: MeetingPlatform::Zoom,
                active: true,
                started_at_unix_ms: 1_700_000_000_000,
                duration_s: None,
                title: "Standup".to_owned(),
                organizer: "host@example.test".to_owned(),
                attendee_count: 3,
                has_calendar_metadata: true,
            }],
            ..MeetingDetectStatus::default()
        };

        let app = router(Arc::clone(&mgr), 9555, meeting_channel_with(meeting));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let document = body_json(response).await;

        assert_eq!(document["meeting"]["state"], "running");
        assert_eq!(document["meeting"]["meeting_state"], "active");
        assert_eq!(document["meeting"]["meetings"][0]["platform"], "zoom");
        assert_eq!(document["meeting"]["meetings"][0]["title"], "Standup");
        assert_eq!(document["meeting"]["meetings"][0]["attendee_count"], 3);
        assert!(
            document["meeting"]["meetings"][0]
                .get("attendees")
                .is_none(),
            "attendee addresses must not be reported"
        );
        assert!(
            document["meeting"].get("socket_path").is_none(),
            "detector socket paths must not be reported"
        );
    }

    #[tokio::test]
    async fn ephemeral_listener_serves_and_shuts_down() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let launcher = Arc::new(FakeNodeLauncher::new());
        let mgr = Arc::new(manager(launcher));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop_tx, stop_rx) = watch::channel(false);

        let task = tokio::spawn(serve(
            Arc::clone(&mgr),
            listener,
            address.port(),
            stop_rx,
            meeting_channel(),
        ));

        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(
            response.contains("\"profile\":\"hypervisor\""),
            "{response}"
        );

        stop_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("status server shuts down")
            .expect("status task")
            .expect("status server result");
    }
}
