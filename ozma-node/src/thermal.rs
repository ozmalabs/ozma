// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Thermal runtime: hwmon discovery, policy loading, control loop, and status API.
//!
//! Fan control is opt-in. Without `--thermal-control` this module is read-only
//! and never writes a PWM value. With control enabled it still requires a
//! policy file, because zone/fan/sensor names and safe limits are
//! machine-specific.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use ozma_thermal::{
    combine_dba, estimate_fan_dba, FanReading, FanStatus, HwmonController, HwmonError,
    HwmonInventory, SensorReading, SensorStatus, ThermalEngine, ThermalPolicy, ThermalStatus,
    ZoneStatus,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

/// Shared status snapshot used by the HTTP routes and the control loop.
pub type ThermalStatusHandle = Arc<RwLock<ThermalStatus>>;

/// Runtime options derived from the node CLI.
#[derive(Clone, Debug)]
pub struct ThermalOptions {
    /// Optional JSON thermal policy. Control requires one.
    pub policy_path: Option<PathBuf>,
    /// Allow PWM writes. Read-only mode is the default.
    pub control: bool,
    /// Poll interval in seconds.
    pub interval_secs: u64,
    /// sysfs hwmon root (overridable for tests/CI).
    pub hwmon_root: PathBuf,
}

impl Default for ThermalOptions {
    fn default() -> Self {
        Self {
            policy_path: None,
            control: false,
            interval_secs: 5,
            hwmon_root: PathBuf::from(ozma_thermal::DEFAULT_HWMON_ROOT),
        }
    }
}

/// Load and validate a thermal policy from JSON.
pub fn load_policy(path: &Path) -> anyhow::Result<ThermalPolicy> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| anyhow::anyhow!("cannot read {}: {error}", path.display()))?;
    let policy: ThermalPolicy = serde_json::from_str(&text)
        .map_err(|error| anyhow::anyhow!("invalid thermal policy {}: {error}", path.display()))?;
    policy
        .validate()
        .map_err(|error| anyhow::anyhow!("invalid thermal policy {}: {error}", path.display()))?;
    Ok(policy)
}

/// Build the authenticated thermal status router.
pub fn router(status: ThermalStatusHandle, policy: Option<Arc<ThermalPolicy>>) -> Router {
    Router::new()
        .route("/thermal", get(status_handler))
        .route("/api/v1/thermal", get(status_handler))
        .route("/api/v1/thermal/status", get(status_handler))
        .route("/api/v1/thermal/zones", get(zones_handler))
        .route("/api/v1/thermal/fans", get(fans_handler))
        .route("/api/v1/thermal/profiles", get(profiles_handler))
        .with_state(ThermalRouterState { status, policy })
}

#[derive(Clone)]
struct ThermalRouterState {
    status: ThermalStatusHandle,
    policy: Option<Arc<ThermalPolicy>>,
}

async fn status_handler(State(state): State<ThermalRouterState>) -> Json<ThermalStatus> {
    Json(read_status(&state.status))
}

async fn zones_handler(State(state): State<ThermalRouterState>) -> Json<Vec<ZoneStatus>> {
    Json(read_status(&state.status).zones)
}

async fn fans_handler(State(state): State<ThermalRouterState>) -> Json<Vec<FanStatus>> {
    Json(read_status(&state.status).fans)
}

async fn profiles_handler(State(state): State<ThermalRouterState>) -> Json<Value> {
    match state.policy {
        Some(policy) => Json(json!({
            "curves": policy.curves,
            "zones": policy.zones,
            "fans": policy.fans,
            "poll_interval_secs": policy.poll_interval_secs,
            "min_dwell_secs": policy.min_dwell_secs,
        })),
        None => Json(json!({
            "curves": [],
            "zones": [],
            "fans": [],
            "note": "no thermal policy loaded; node is read-only",
        })),
    }
}

fn read_status(handle: &ThermalStatusHandle) -> ThermalStatus {
    handle
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Run the thermal loop until cancellation.
///
/// Without a policy this samples hwmon and publishes a read-only status. With a
/// policy it runs the fan-curve engine. PWM writes happen only when `control`
/// is true, an inventory fan has a PWM control, and the policy loaded
/// successfully.
pub async fn run(options: ThermalOptions, status: ThermalStatusHandle, cancel: CancellationToken) {
    let policy = match options.policy_path.as_deref() {
        Some(path) => match load_policy(path) {
            Ok(policy) => Some(policy),
            Err(error) => {
                error!(error = %error, "thermal policy could not be loaded; staying read-only");
                None
            }
        },
        None => None,
    };

    if options.control && policy.is_none() {
        warn!(
            "thermal control was requested without a policy; \
             refusing to write fan PWM"
        );
    }

    let mut worker = ThermalWorker::new(options.clone(), policy);
    worker.publish(&status);

    let interval = Duration::from_secs(options.interval_secs.max(1));
    loop {
        if cancel.is_cancelled() {
            break;
        }
        let started = std::time::Instant::now();
        worker.sample_and_publish(&status);
        let elapsed = started.elapsed();
        let delay = interval.saturating_sub(elapsed);
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(delay) => {}
        }
    }

    let control_was_active = worker
        .controller
        .as_ref()
        .map(ozma_thermal::HwmonController::manual)
        .unwrap_or(false);
    worker.stop();
    let mut final_status = read_status(&status);
    final_status.control_enabled = false;
    if control_was_active {
        final_status
            .warnings
            .push("thermal control stopped; fan modes restored".to_owned());
    }
    final_status.updated_at = unix_timestamp();
    if let Ok(mut guard) = status.write() {
        *guard = final_status;
    }
}

struct ThermalWorker {
    options: ThermalOptions,
    policy: Option<ThermalPolicy>,
    engine: Option<ThermalEngine>,
    inventory: HwmonInventory,
    controller: Option<HwmonController>,
    control_failed: bool,
}

impl ThermalWorker {
    fn new(options: ThermalOptions, policy: Option<ThermalPolicy>) -> Self {
        let engine = policy
            .as_ref()
            .and_then(|policy| match ThermalEngine::new(policy.clone()) {
                Ok(engine) => Some(engine),
                Err(error) => {
                    error!(error = %error, "thermal engine validation failed");
                    None
                }
            });
        let inventory = match HwmonInventory::discover(&options.hwmon_root) {
            Ok(inventory) => inventory,
            Err(error) => {
                warn!(
                    root = %options.hwmon_root.display(),
                    error = %error,
                    "hwmon discovery failed; thermal status will be empty"
                );
                HwmonInventory::default()
            }
        };
        Self {
            options,
            policy,
            engine,
            inventory,
            controller: None,
            control_failed: false,
        }
    }

    fn publish(&mut self, status: &ThermalStatusHandle) {
        let snapshot = self.snapshot();
        if let Ok(mut guard) = status.write() {
            *guard = snapshot;
        }
    }

    fn sample_and_publish(&mut self, status: &ThermalStatusHandle) {
        self.publish(status);
    }

    fn snapshot(&mut self) -> ThermalStatus {
        let now = unix_timestamp();
        let mut warnings = Vec::new();
        let sensors = self.inventory.read_sensors().unwrap_or_else(|error| {
            warnings.push(format!("cannot read temperature sensors: {error}"));
            Vec::new()
        });
        let fans = self.inventory.read_fans().unwrap_or_else(|error| {
            warnings.push(format!("cannot read fan speeds: {error}"));
            Vec::new()
        });

        let mut snapshot = ThermalStatus {
            ok: true,
            control_enabled: false,
            policy_loaded: self.policy.is_some() && self.engine.is_some(),
            sensors: sensors
                .iter()
                .map(|sensor| SensorStatus {
                    id: sensor.id.clone(),
                    name: sensor.name.clone(),
                    temp_c: sensor.temp_c,
                    age_secs: sensor.age_secs,
                })
                .collect(),
            fans: fans
                .iter()
                .map(|fan| FanStatus {
                    id: fan.id.clone(),
                    name: fan.name.clone(),
                    rpm: fan.rpm,
                    duty_percent: fan.duty_percent,
                    target_duty_percent: None,
                    controllable: fan.controllable,
                    estimated_dba: None,
                })
                .collect(),
            zones: Vec::new(),
            estimated_dba: None,
            shutdown_requested: false,
            warnings,
            updated_at: now,
        };

        let (Some(policy), Some(engine)) = (self.policy.clone(), self.engine.as_mut()) else {
            snapshot.warnings.push(if self.options.control {
                "thermal control requested without a valid policy; read-only".to_owned()
            } else {
                "fan control disabled; exposing read-only status".to_owned()
            });
            return snapshot;
        };

        let sensor_map: BTreeMap<String, SensorReading> = sensors
            .iter()
            .map(|sensor| (sensor.id.clone(), sensor.clone()))
            .collect();
        let fan_map: BTreeMap<String, FanReading> = fans
            .iter()
            .map(|fan| (fan.id.clone(), fan.clone()))
            .collect();
        let plan = engine.tick(now, &sensor_map, &fan_map);
        snapshot.estimated_dba = plan.estimated_dba;
        snapshot.warnings.extend(plan.warnings.iter().cloned());
        snapshot.zones = plan.zones.clone();
        snapshot.shutdown_requested = plan.shutdown_requested;

        let mut targets: BTreeMap<String, f64> = BTreeMap::new();
        for command in &plan.fans {
            targets.insert(command.fan_id.clone(), command.duty_percent);
        }
        for fan in &mut snapshot.fans {
            fan.target_duty_percent = targets.get(&fan.id).copied();
            if let (Some(spec), Some(duty)) = (
                policy.fans.iter().find(|spec| spec.id == fan.id),
                fan.target_duty_percent,
            ) {
                fan.estimated_dba = estimate_fan_dba(spec, duty);
            }
        }

        if self.control_failed {
            snapshot
                .warnings
                .push("fan control disabled after a PWM error".to_owned());
        } else if self.options.control {
            self.apply_plan(&policy, &plan, &mut snapshot);
        } else {
            snapshot
                .warnings
                .push("fan control disabled; exposing read-only status".to_owned());
        }

        if plan.shutdown_requested {
            error!("thermal emergency shutdown threshold reached");
            snapshot
                .warnings
                .push("emergency shutdown threshold reached".to_owned());
        }

        snapshot
    }

    fn apply_plan(
        &mut self,
        policy: &ThermalPolicy,
        plan: &ozma_thermal::ThermalPlan,
        snapshot: &mut ThermalStatus,
    ) {
        if plan.fans.is_empty() {
            return;
        }
        let inventory = self.inventory.clone();
        let controller = self
            .controller
            .get_or_insert_with(|| HwmonController::new(inventory));

        for command in &plan.fans {
            let Some(spec) = policy.fans.iter().find(|spec| spec.id == command.fan_id) else {
                continue;
            };
            if !spec.controllable {
                continue;
            }
            if let Err(error) = controller.apply(&command.fan_id, command.duty_percent) {
                match error {
                    HwmonError::NotControllable(_) => {}
                    other => {
                        self.control_failed = true;
                        warn!(fan = %command.fan_id, error = %other, "fan PWM write failed");
                        let _ = controller.restore();
                        snapshot
                            .warnings
                            .push(format!("fan {} PWM write failed: {other}", command.fan_id));
                        break;
                    }
                }
            }
        }
        snapshot.control_enabled = !self.control_failed;
    }

    fn stop(&mut self) {
        if let Some(controller) = self.controller.as_mut() {
            if let Err(error) = controller.restore() {
                warn!(error = %error, "failed to restore automatic fan control");
            }
        }
    }
}

/// Combined dBA for a set of `(fan_id, duty)` commands.
pub fn combined_estimate(
    policy: &ThermalPolicy,
    commands: impl IntoIterator<Item = (String, f64)>,
) -> Option<f64> {
    let mut levels = Vec::new();
    for (fan_id, duty) in commands {
        let Some(spec) = policy.fans.iter().find(|spec| spec.id == fan_id) else {
            continue;
        };
        let Some(dba) = estimate_fan_dba(spec, duty) else {
            continue;
        };
        for _ in 0..spec.count.max(1) {
            levels.push(dba);
        }
    }
    combine_dba(levels)
}

fn unix_timestamp() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_thermal::{FanCurve, FanCurvePoint, FanSpec, ThermalLimits, ThermalZone};

    fn policy() -> ThermalPolicy {
        ThermalPolicy {
            zones: vec![ThermalZone {
                id: "cpu".into(),
                name: "CPU".into(),
                sensor_ids: vec!["hwmon:coretemp:temp1".into()],
                fan_ids: vec!["hwmon:it87:pwm1".into()],
                curve_id: Some("cpu".into()),
                limits: ThermalLimits {
                    safe_c: 80.0,
                    warning_c: 88.0,
                    critical_c: 95.0,
                    shutdown_c: None,
                },
            }],
            curves: vec![FanCurve {
                id: "cpu".into(),
                name: "CPU".into(),
                zone: Some("cpu".into()),
                fan_ids: vec![],
                sensor_ids: vec![],
                sensor_mode: Default::default(),
                sensor_weights: BTreeMap::new(),
                points: vec![
                    FanCurvePoint {
                        temp_c: 40.0,
                        duty_percent: 30.0,
                    },
                    FanCurvePoint {
                        temp_c: 80.0,
                        duty_percent: 100.0,
                    },
                ],
                hysteresis_c: 2.0,
                ramp_rate_percent_per_sec: None,
                min_duty_percent: 20.0,
                max_duty_percent: 100.0,
                critical_override: None,
                noise_cap: None,
            }],
            fans: vec![FanSpec {
                id: "hwmon:it87:pwm1".into(),
                name: "CPU fan".into(),
                count: 1,
                min_rpm: Some(400),
                max_rpm: Some(1600),
                noise_dba: Some(30.0),
                noise_at_idle_dba: Some(16.0),
                noise_points: vec![],
                location: None,
                controllable: true,
            }],
            ..ThermalPolicy::default()
        }
    }

    #[test]
    fn router_builds_without_a_policy() {
        let _ = router(Arc::new(RwLock::new(ThermalStatus::default())), None);
    }

    #[test]
    fn combined_estimate_uses_fan_count_and_acoustics() {
        let policy = policy();
        let dba = combined_estimate(&policy, [("hwmon:it87:pwm1".to_string(), 100.0)]).unwrap();
        assert!((dba - 30.0).abs() < 0.01);
    }

    #[test]
    fn load_policy_rejects_missing_file() {
        assert!(load_policy(Path::new("/nonexistent/ozma-thermal.json")).is_err());
    }

    #[test]
    fn thermal_options_default_is_read_only() {
        let options = ThermalOptions::default();
        assert!(!options.control);
        assert_eq!(options.interval_secs, 5);
    }
}
