// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Read-only persisted multi-seat plan/status for `GET /api/v1/seats`.
//!
//! `ozma-node` does not own seat lifecycle. The standalone `ozma-multiseat`
//! supervisor starts, health-checks, restarts, and stops one node child per
//! seat. This module only lets a node expose the persisted plan it was launched
//! from (`--seats-file`) so the controller can group seats without probing
//! node argv, environment, or the supervisor's private status port.
//!
//! Opt-in and off by default:
//!
//! * Without `--multiseat` the route reports `configured:false` and no seats.
//! * With `--multiseat --seats-file PATH` the node loads the versioned
//!   `SeatStore` once at startup and reports the persisted identities.
//! * No route here starts or stops a seat process. `lifecycle` is always
//!   `"external"` and `start_stop` is always `false`.
//! * `seat_id` is only ever the explicit `OZMA_SEAT_ID` environment value. The
//!   node never derives or invents one from its name.
//! * A store read/parse/version error is a typed 503 document; the node keeps
//!   running and does not serve fabricated seats.

use std::path::Path;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use axum::Router;
use ozma_multiseat::{
    MultiseatError, PersistedSeatStatus, SeatStore, SeatStoreStatus, SEAT_STORE_VERSION,
};
use serde::Serialize;

/// Controller-facing read-only seat report path.
pub const SEATS_PATH: &str = "/api/v1/seats";

/// Lifecycle owner reported for every seat. Process lifecycle stays with the
/// standalone `ozma-multiseat` supervisor, never this node.
pub const LIFECYCLE_EXTERNAL: &str = "external";

/// Not-configured mode name.
pub const MODE_NOT_CONFIGURED: &str = "not-configured";
/// Read-only persisted-plan mode name.
pub const MODE_READ_ONLY: &str = "read-only";
/// Typed seat-store failure mode name.
pub const MODE_ERROR: &str = "error";

/// Immutable state behind [`SEATS_PATH`].
#[derive(Clone, Debug)]
pub struct SeatReportState {
    inner: Arc<SeatReport>,
}

#[derive(Debug)]
enum SeatReport {
    NotConfigured,
    Loaded {
        source: String,
        seat_id: Option<String>,
        plan: SeatStore,
        status: SeatStoreStatus,
        node_seat: Option<PersistedSeatStatus>,
    },
    Failed {
        source: String,
        seat_id: Option<String>,
        code: String,
        message: String,
    },
}

impl SeatReportState {
    /// The route is present but reporting is disabled.
    pub fn not_configured() -> Self {
        Self {
            inner: Arc::new(SeatReport::NotConfigured),
        }
    }

    /// Load a persisted seat store for read-only reporting.
    ///
    /// `seat_id` must come from the explicit `OZMA_SEAT_ID` environment value
    /// (see [`crate::registration::seat_id_from_env`]); this function never
    /// derives an id. `discovered_displays` is the set of outputs this node can
    /// actually observe, used only to mark a retained seat's display missing.
    /// Pass an empty slice when the node did not enumerate displays.
    pub fn load(
        path: impl AsRef<Path>,
        seat_id: Option<String>,
        discovered_displays: &[String],
    ) -> Self {
        let path = path.as_ref();
        let source = path.display().to_string();
        match SeatStore::load(path) {
            Ok(plan) => Self::from_store(plan, source, seat_id, discovered_displays),
            Err(error) => Self::failed(source, seat_id, &error),
        }
    }

    /// Build a report from an already-loaded store. Used by tests and callers
    /// that own store loading.
    pub fn from_store(
        plan: SeatStore,
        source: impl Into<String>,
        seat_id: Option<String>,
        discovered_displays: &[String],
    ) -> Self {
        let source = source.into();
        let seat_id = seat_id.and_then(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        });
        let status = plan.status(discovered_displays);
        let node_seat = seat_id
            .as_deref()
            .and_then(|id| status.seats.iter().find(|seat| seat.seat_id == id).cloned());
        Self {
            inner: Arc::new(SeatReport::Loaded {
                source,
                seat_id,
                plan,
                status,
                node_seat,
            }),
        }
    }

    /// Build a typed failure report. The message is the library error text; the
    /// code is stable and path-free.
    pub fn failed(
        source: impl Into<String>,
        seat_id: Option<String>,
        error: &MultiseatError,
    ) -> Self {
        let seat_id = seat_id.and_then(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        });
        Self {
            inner: Arc::new(SeatReport::Failed {
                source: source.into(),
                seat_id,
                code: error.code().to_owned(),
                message: error.to_string(),
            }),
        }
    }

    /// A stable, path-free summary for logs.
    pub fn summary(&self) -> SeatReportSummary {
        match self.inner.as_ref() {
            SeatReport::NotConfigured => SeatReportSummary {
                configured: false,
                mode: MODE_NOT_CONFIGURED,
                seat_count: 0,
                code: None,
            },
            SeatReport::Loaded { plan, .. } => SeatReportSummary {
                configured: true,
                mode: MODE_READ_ONLY,
                seat_count: plan.seats.len(),
                code: None,
            },
            SeatReport::Failed { code, .. } => SeatReportSummary {
                configured: true,
                mode: MODE_ERROR,
                seat_count: 0,
                code: Some(code.clone()),
            },
        }
    }
}

/// Compact report summary for startup logging.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatReportSummary {
    pub configured: bool,
    pub mode: &'static str,
    pub seat_count: usize,
    pub code: Option<String>,
}

/// Build the unauthenticated read-only seat router.
///
/// Merge this router after the node API's bearer layer, next to
/// [`crate::controller_compat::public_router`]. It exposes no control surface
/// and never starts or stops a process.
pub fn router(state: SeatReportState) -> Router {
    Router::new()
        .route(SEATS_PATH, get(seats_report))
        .with_state(state)
}

/// Response for `GET /api/v1/seats`.
///
/// `ok` is `true` for both not-configured and read-only reports; it is `false`
/// only for a typed seat-store failure. `plan` is the persisted store verbatim.
/// `status` is the path-free per-seat status built by
/// [`ozma_multiseat::SeatStore::status`].
#[derive(Clone, Debug, Serialize)]
pub struct SeatsResponse {
    /// Whether the document describes a usable read-only report.
    pub ok: bool,
    /// Whether `--multiseat --seats-file` configured the report.
    pub configured: bool,
    /// `not-configured`, `read-only`, or `error`.
    pub mode: &'static str,
    /// Lifecycle owner. Always `external`: the standalone supervisor owns
    /// start/stop/restart.
    pub lifecycle: &'static str,
    /// Always `false`; this node exposes no seat start/stop route.
    pub start_stop: bool,
    /// Persisted store path, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Explicit seat id from `OZMA_SEAT_ID`, when present. Never invented.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seat_id: Option<String>,
    /// Persisted plan (the versioned seat store), when loaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<SeatStore>,
    /// Read-only per-seat status, when loaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<SeatStoreStatus>,
    /// This node's persisted seat, when `seat_id` names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_seat: Option<PersistedSeatStatus>,
    /// Typed failure, when the seat store could not be loaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<SeatsErrorBody>,
}

/// Typed seat-store failure body.
#[derive(Clone, Debug, Serialize)]
pub struct SeatsErrorBody {
    /// Stable, path-free error code (for example `seat_store_parse`).
    pub code: String,
    /// Human-readable error text.
    pub message: String,
}

impl SeatsResponse {
    fn not_configured() -> Self {
        Self {
            ok: true,
            configured: false,
            mode: MODE_NOT_CONFIGURED,
            lifecycle: LIFECYCLE_EXTERNAL,
            start_stop: false,
            source: None,
            seat_id: None,
            plan: None,
            status: None,
            node_seat: None,
            error: None,
        }
    }
}

async fn seats_report(State(state): State<SeatReportState>) -> Response {
    match state.inner.as_ref() {
        SeatReport::NotConfigured => Json(SeatsResponse::not_configured()).into_response(),
        SeatReport::Loaded {
            source,
            seat_id,
            plan,
            status,
            node_seat,
        } => Json(SeatsResponse {
            ok: true,
            configured: true,
            mode: MODE_READ_ONLY,
            lifecycle: LIFECYCLE_EXTERNAL,
            start_stop: false,
            source: Some(source.clone()),
            seat_id: seat_id.clone(),
            plan: Some(plan.clone()),
            status: Some(status.clone()),
            node_seat: node_seat.clone(),
            error: None,
        })
        .into_response(),
        SeatReport::Failed {
            source,
            seat_id,
            code,
            message,
        } => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(SeatsResponse {
                ok: false,
                configured: true,
                mode: MODE_ERROR,
                lifecycle: LIFECYCLE_EXTERNAL,
                start_stop: false,
                source: Some(source.clone()),
                seat_id: seat_id.clone(),
                plan: None,
                status: None,
                node_seat: None,
                error: Some(SeatsErrorBody {
                    code: code.clone(),
                    message: message.clone(),
                }),
            }),
        )
            .into_response(),
    }
}

/// Build an empty current-version store for tests and callers without a file.
pub fn empty_store() -> SeatStore {
    SeatStore {
        version: SEAT_STORE_VERSION,
        seats: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use ozma_multiseat::{PersistedDisplay, PersistedSeat, SeatProfile, SeatState};
    use serde_json::Value;
    use tower::ServiceExt;

    fn seat(id: &str, output: &str, index: u32) -> PersistedSeat {
        PersistedSeat {
            id: id.to_owned(),
            display: PersistedDisplay {
                output: output.to_owned(),
                index,
                width: 1920,
                height: 1080,
                x: index as i32 * 1920,
                y: 0,
                x_screen: ":0".to_owned(),
            },
            input_hub: Some("1-1".to_owned()),
            input_devices: vec!["/dev/input/event0".to_owned()],
            audio_sink: format!("ozma-seat-{index}"),
            udp_port: 7331 + index as u16,
            http_port: 7382 + index as u16,
            profile: SeatProfile::Desktop,
        }
    }

    fn store() -> SeatStore {
        SeatStore {
            version: SEAT_STORE_VERSION,
            seats: vec![seat("host-seat-0", "HDMI-1", 0)],
        }
    }

    async fn get_json(app: &Router, uri: &str) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn not_configured_reports_no_seats_and_no_process_ownership() {
        let (status, json) = get_json(&router(SeatReportState::not_configured()), SEATS_PATH).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["ok"], true);
        assert_eq!(json["configured"], false);
        assert_eq!(json["mode"], MODE_NOT_CONFIGURED);
        assert_eq!(json["lifecycle"], LIFECYCLE_EXTERNAL);
        assert_eq!(json["start_stop"], false);
        assert!(json.get("plan").is_none());
        assert!(json.get("status").is_none());
        assert!(json.get("seat_id").is_none());
    }

    #[tokio::test]
    async fn persisted_plan_is_reported_read_only_with_the_explicit_seat_id() {
        let state = SeatReportState::from_store(
            store(),
            "/tmp/seats.json",
            Some("host-seat-0".to_owned()),
            &[],
        );

        let (status, json) = get_json(&router(state), SEATS_PATH).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["configured"], true);
        assert_eq!(json["mode"], MODE_READ_ONLY);
        assert_eq!(json["lifecycle"], LIFECYCLE_EXTERNAL);
        assert_eq!(json["start_stop"], false);
        assert_eq!(json["seat_id"], "host-seat-0");
        assert_eq!(json["plan"]["version"], SEAT_STORE_VERSION);
        assert_eq!(json["plan"]["seats"][0]["id"], "host-seat-0");
        assert_eq!(json["status"]["seat_count"], 1);
        assert_eq!(json["status"]["node_count"], 1);
        assert_eq!(json["status"]["seats"][0]["state"], "planned");
        assert_eq!(json["node_seat"]["seat_id"], "host-seat-0");
        assert_eq!(json["node_seat"]["display_output"], "HDMI-1");
    }

    #[tokio::test]
    async fn missing_display_is_retained_as_planned_with_a_typed_diagnostic() {
        let mut plan = store();
        plan.seats.push(seat("host-seat-1", "DP-2", 1));
        let state =
            SeatReportState::from_store(plan, "/tmp/seats.json", None, &["HDMI-1".to_owned()]);

        let (status, json) = get_json(&router(state), SEATS_PATH).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["status"]["seat_count"], 2);
        assert_eq!(json["status"]["seats"][0]["available"], true);
        assert_eq!(json["status"]["seats"][1]["available"], false);
        assert_eq!(json["status"]["seats"][1]["state"], "planned");
        assert_eq!(
            json["status"]["diagnostics"][0]["code"],
            "persisted_seat_missing_display"
        );
        assert_eq!(json["status"]["diagnostics"][0]["seat_id"], "host-seat-1");
        // Read-only plan mode never claims a running child.
        assert!(json["status"]["seats"][1].get("pid").is_none());
    }

    #[tokio::test]
    async fn parse_error_is_a_typed_503_without_fabricated_seats() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("seats.json");
        std::fs::write(&path, "{ not json").unwrap();
        let state = SeatReportState::load(&path, Some("host-seat-0".to_owned()), &[]);

        let (status, json) = get_json(&router(state), SEATS_PATH).await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json["ok"], false);
        assert_eq!(json["configured"], true);
        assert_eq!(json["mode"], MODE_ERROR);
        assert_eq!(json["lifecycle"], LIFECYCLE_EXTERNAL);
        assert_eq!(json["error"]["code"], "seat_store_parse");
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("seats.json"));
        assert!(json.get("plan").is_none());
        assert!(json.get("status").is_none());
        assert!(json.get("node_seat").is_none());
    }

    #[tokio::test]
    async fn version_error_is_typed_and_path_free_in_the_code() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("seats.json");
        std::fs::write(
            &path,
            format!("{{\"version\": {}, \"seats\": []}}", SEAT_STORE_VERSION + 1),
        )
        .unwrap();
        let state = SeatReportState::load(&path, None, &[]);

        let (status, json) = get_json(&router(state), SEATS_PATH).await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json["error"]["code"], "seat_store_version");
    }

    #[test]
    fn loaded_status_is_planned_and_never_invents_a_seat_id() {
        let state = SeatReportState::from_store(store(), "/tmp/seats.json", None, &[]);
        let summary = state.summary();
        assert_eq!(summary.mode, MODE_READ_ONLY);
        assert_eq!(summary.seat_count, 1);

        let store = state.inner.as_ref();
        match store {
            SeatReport::Loaded {
                seat_id, status, ..
            } => {
                assert!(seat_id.is_none());
                assert_eq!(status.seats[0].state, SeatState::Planned);
            }
            other => panic!("unexpected report: {other:?}"),
        }
    }

    #[test]
    fn empty_store_helper_is_current_version_and_empty() {
        let store = empty_store();
        assert_eq!(store.version, SEAT_STORE_VERSION);
        assert!(store.seats.is_empty());
    }
}
