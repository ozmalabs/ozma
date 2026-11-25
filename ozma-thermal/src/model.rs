// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Thermal policy, sensor, fan, and status types.
//!
//! The model follows `docs/routing/thermal.md`: a zone owns sensors and fans, a
//! fan curve maps a representative temperature to duty, and a critical override
//! is always allowed to ignore the curve, noise cap, and ramp limit.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

fn default_hysteresis() -> f64 {
    2.0
}

fn default_max_duty() -> f64 {
    100.0
}

fn default_poll_secs() -> u64 {
    5
}

fn default_min_dwell_secs() -> f64 {
    30.0
}

fn default_max_sensor_age_secs() -> f64 {
    15.0
}

fn default_failsafe_duty() -> f64 {
    70.0
}

fn default_critical_action() -> CriticalAction {
    CriticalAction::MaxSpeed
}

fn default_sensor_mode() -> SensorMode {
    SensorMode::Max
}

/// How a zone combines multiple sensor readings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensorMode {
    /// The hottest valid sensor drives the zone.
    #[default]
    Max,
    /// Arithmetic mean of all valid sensors.
    Average,
    /// Weighted mean using [`FanCurve::sensor_weights`].
    Weighted,
}

/// What to do when a zone reaches its critical threshold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriticalAction {
    /// Run every fan in the zone at maximum speed.
    #[default]
    MaxSpeed,
    /// Request throttling where the platform supports it and run fans at max.
    ThrottleAndMax,
    /// Run fans at max and request an emergency shutdown.
    Shutdown,
}

/// Health of a temperature zone at the last sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThermalState {
    /// No fresh sensor reading.
    #[default]
    Unknown,
    /// At or below the safe threshold.
    Normal,
    /// Above safe but below critical.
    Warning,
    /// At or above the critical threshold.
    Critical,
    /// At or above the shutdown threshold.
    Shutdown,
}

/// Safe operating thresholds for one thermal zone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThermalLimits {
    /// Temperature at or below which the zone is comfortable.
    pub safe_c: f64,
    /// Temperature where noise limits may be relaxed to protect the hardware.
    pub warning_c: f64,
    /// Temperature where the critical override runs fans at maximum.
    pub critical_c: f64,
    /// Optional temperature where an emergency shutdown is requested.
    #[serde(default)]
    pub shutdown_c: Option<f64>,
}

impl ThermalLimits {
    /// Validate ordering and plausible operating range.
    pub fn validate(&self) -> Result<(), String> {
        if !self.safe_c.is_finite() || !self.warning_c.is_finite() || !self.critical_c.is_finite() {
            return Err("thermal limits must be finite".to_owned());
        }
        if self.safe_c < -40.0 || self.critical_c > 150.0 {
            return Err("thermal limits are outside a plausible operating range".to_owned());
        }
        if !(self.safe_c < self.warning_c && self.warning_c < self.critical_c) {
            return Err(format!(
                "thermal limits must satisfy safe < warning < critical, got {} < {} < {}",
                self.safe_c, self.warning_c, self.critical_c
            ));
        }
        if let Some(shutdown) = self.shutdown_c {
            if !shutdown.is_finite() || shutdown <= self.critical_c {
                return Err("shutdown_c must be finite and above critical_c".to_owned());
            }
        }
        Ok(())
    }

    /// Classify a temperature against these limits.
    pub fn state_for(&self, temperature_c: f64) -> ThermalState {
        if let Some(shutdown) = self.shutdown_c {
            if temperature_c >= shutdown {
                return ThermalState::Shutdown;
            }
        }
        if temperature_c >= self.critical_c {
            ThermalState::Critical
        } else if temperature_c >= self.warning_c {
            ThermalState::Warning
        } else {
            ThermalState::Normal
        }
    }
}

/// One point on a fan curve.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FanCurvePoint {
    pub temp_c: f64,
    pub duty_percent: f64,
}

/// Noise ceiling that applies while the zone is not in a warning/critical state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NoiseCap {
    /// Maximum estimated dBA considered comfortable.
    pub comfort_dba: f64,
    /// Maximum estimated dBA allowed once the zone reaches its warning threshold.
    pub warning_dba: f64,
}

/// A named temperature-to-duty curve.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FanCurve {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub zone: Option<String>,
    #[serde(default)]
    pub fan_ids: Vec<String>,
    #[serde(default)]
    pub sensor_ids: Vec<String>,
    #[serde(default = "default_sensor_mode")]
    pub sensor_mode: SensorMode,
    #[serde(default)]
    pub sensor_weights: BTreeMap<String, f64>,
    pub points: Vec<FanCurvePoint>,
    #[serde(default = "default_hysteresis")]
    pub hysteresis_c: f64,
    #[serde(default)]
    pub ramp_rate_percent_per_sec: Option<f64>,
    #[serde(default)]
    pub min_duty_percent: f64,
    #[serde(default = "default_max_duty")]
    pub max_duty_percent: f64,
    #[serde(default)]
    pub critical_override: Option<CriticalOverride>,
    #[serde(default)]
    pub noise_cap: Option<NoiseCap>,
}

impl FanCurve {
    /// Validate curve points, duty bounds, and optional control parameters.
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("fan curve id must not be empty".to_owned());
        }
        if self.points.is_empty() {
            return Err(format!("fan curve {:?} has no points", self.id));
        }
        if !self.hysteresis_c.is_finite() || self.hysteresis_c < 0.0 {
            return Err(format!(
                "fan curve {:?} hysteresis must be finite and non-negative",
                self.id
            ));
        }
        if !(0.0..=100.0).contains(&self.min_duty_percent)
            || !(0.0..=100.0).contains(&self.max_duty_percent)
            || self.min_duty_percent > self.max_duty_percent
        {
            return Err(format!(
                "fan curve {:?} duty bounds must satisfy 0 <= min <= max <= 100",
                self.id
            ));
        }
        if let Some(ramp) = self.ramp_rate_percent_per_sec {
            if !ramp.is_finite() || ramp <= 0.0 {
                return Err(format!(
                    "fan curve {:?} ramp rate must be finite and positive",
                    self.id
                ));
            }
        }
        let mut previous_temp = f64::NEG_INFINITY;
        for point in &self.points {
            if !point.temp_c.is_finite() || !point.duty_percent.is_finite() {
                return Err(format!("fan curve {:?} has a non-finite point", self.id));
            }
            if !(0.0..=100.0).contains(&point.duty_percent) {
                return Err(format!(
                    "fan curve {:?} duty point {} is outside 0..=100",
                    self.id, point.duty_percent
                ));
            }
            if point.temp_c <= previous_temp {
                return Err(format!(
                    "fan curve {:?} points must be strictly ordered by temperature",
                    self.id
                ));
            }
            previous_temp = point.temp_c;
        }
        if let Some(cap) = &self.noise_cap {
            if !cap.comfort_dba.is_finite()
                || !cap.warning_dba.is_finite()
                || cap.comfort_dba < 0.0
                || cap.warning_dba < cap.comfort_dba
            {
                return Err(format!(
                    "fan curve {:?} noise cap must satisfy 0 <= comfort <= warning",
                    self.id
                ));
            }
        }
        if let Some(critical) = &self.critical_override {
            if !critical.threshold_c.is_finite() {
                return Err(format!(
                    "fan curve {:?} critical threshold must be finite",
                    self.id
                ));
            }
            if let Some(shutdown) = critical.shutdown_c {
                if !shutdown.is_finite() || shutdown <= critical.threshold_c {
                    return Err(format!(
                        "fan curve {:?} shutdown threshold must be above critical",
                        self.id
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Critical override embedded in a fan curve.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CriticalOverride {
    pub threshold_c: f64,
    #[serde(default)]
    pub shutdown_c: Option<f64>,
    #[serde(default = "default_critical_action")]
    pub action: CriticalAction,
}

/// A point on a fan's acoustic curve.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NoisePoint {
    pub rpm: f64,
    pub dba: f64,
}

/// Manufacturer or measured specification for one fan model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FanSpec {
    pub id: String,
    pub name: String,
    #[serde(default = "default_fan_count")]
    pub count: u32,
    #[serde(default)]
    pub min_rpm: Option<u32>,
    #[serde(default)]
    pub max_rpm: Option<u32>,
    #[serde(default)]
    pub noise_dba: Option<f64>,
    #[serde(default)]
    pub noise_at_idle_dba: Option<f64>,
    #[serde(default)]
    pub noise_points: Vec<NoisePoint>,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub controllable: bool,
}

fn default_fan_count() -> u32 {
    1
}

impl FanSpec {
    /// Validate RPM and acoustic fields.
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("fan spec id must not be empty".to_owned());
        }
        if self.count == 0 {
            return Err(format!("fan spec {:?} count must be positive", self.id));
        }
        if let (Some(min), Some(max)) = (self.min_rpm, self.max_rpm) {
            if min >= max {
                return Err(format!(
                    "fan spec {:?} min_rpm must be below max_rpm",
                    self.id
                ));
            }
        }
        for (label, value) in [
            ("noise_dba", self.noise_dba),
            ("noise_at_idle_dba", self.noise_at_idle_dba),
        ] {
            if let Some(value) = value {
                if !value.is_finite() || !(0.0..=120.0).contains(&value) {
                    return Err(format!(
                        "fan spec {:?} {label} must be a plausible dBA value",
                        self.id
                    ));
                }
            }
        }
        let mut previous_rpm = f64::NEG_INFINITY;
        for point in &self.noise_points {
            if !point.rpm.is_finite() || !point.dba.is_finite() || point.rpm <= 0.0 {
                return Err(format!(
                    "fan spec {:?} has a non-finite noise point",
                    self.id
                ));
            }
            if point.rpm <= previous_rpm {
                return Err(format!(
                    "fan spec {:?} noise points must be strictly ordered by rpm",
                    self.id
                ));
            }
            previous_rpm = point.rpm;
        }
        Ok(())
    }
}

/// A thermal zone: sensors, fans, limits, and the curve that drives them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThermalZone {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub sensor_ids: Vec<String>,
    #[serde(default)]
    pub fan_ids: Vec<String>,
    #[serde(default)]
    pub curve_id: Option<String>,
    pub limits: ThermalLimits,
}

impl ThermalZone {
    /// Validate zone identity and required policy references.
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("thermal zone id must not be empty".to_owned());
        }
        if self.fan_ids.is_empty() {
            return Err(format!("thermal zone {:?} has no fans", self.id));
        }
        if self.sensor_ids.is_empty() {
            return Err(format!("thermal zone {:?} has no sensors", self.id));
        }
        self.limits.validate()
    }
}

/// Complete thermal policy, normally loaded from JSON.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ThermalPolicy {
    #[serde(default)]
    pub zones: Vec<ThermalZone>,
    #[serde(default)]
    pub curves: Vec<FanCurve>,
    #[serde(default)]
    pub fans: Vec<FanSpec>,
    #[serde(default = "default_poll_secs")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_min_dwell_secs")]
    pub min_dwell_secs: f64,
    #[serde(default = "default_max_sensor_age_secs")]
    pub max_sensor_age_secs: f64,
    #[serde(default = "default_failsafe_duty")]
    pub failsafe_duty_percent: f64,
}

impl ThermalPolicy {
    /// Validate the whole policy: ids, references, curves, and limits.
    pub fn validate(&self) -> Result<(), String> {
        if self.poll_interval_secs == 0 {
            return Err("poll_interval_secs must be positive".to_owned());
        }
        if !self.min_dwell_secs.is_finite() || self.min_dwell_secs < 0.0 {
            return Err("min_dwell_secs must be finite and non-negative".to_owned());
        }
        if !self.max_sensor_age_secs.is_finite() || self.max_sensor_age_secs <= 0.0 {
            return Err("max_sensor_age_secs must be finite and positive".to_owned());
        }
        if !(0.0..=100.0).contains(&self.failsafe_duty_percent) {
            return Err("failsafe_duty_percent must be within 0..=100".to_owned());
        }

        let mut ids = std::collections::BTreeSet::new();
        for zone in &self.zones {
            zone.validate()?;
            if !ids.insert(format!("zone:{}", zone.id)) {
                return Err(format!("duplicate thermal zone id {:?}", zone.id));
            }
        }
        for curve in &self.curves {
            curve.validate()?;
            if !ids.insert(format!("curve:{}", curve.id)) {
                return Err(format!("duplicate fan curve id {:?}", curve.id));
            }
        }
        for fan in &self.fans {
            fan.validate()?;
            if !ids.insert(format!("fan:{}", fan.id)) {
                return Err(format!("duplicate fan spec id {:?}", fan.id));
            }
        }

        let fan_ids: std::collections::BTreeSet<&str> =
            self.fans.iter().map(|fan| fan.id.as_str()).collect();
        let curve_ids: std::collections::BTreeSet<&str> =
            self.curves.iter().map(|curve| curve.id.as_str()).collect();
        let zone_ids: std::collections::BTreeSet<&str> =
            self.zones.iter().map(|zone| zone.id.as_str()).collect();

        for zone in &self.zones {
            for fan_id in &zone.fan_ids {
                if !fan_ids.contains(fan_id.as_str()) {
                    return Err(format!(
                        "thermal zone {:?} references unknown fan {:?}",
                        zone.id, fan_id
                    ));
                }
            }
            match &zone.curve_id {
                Some(curve_id) if !curve_ids.contains(curve_id.as_str()) => {
                    return Err(format!(
                        "thermal zone {:?} references unknown curve {:?}",
                        zone.id, curve_id
                    ));
                }
                Some(_) => {}
                None => {
                    if !self
                        .curves
                        .iter()
                        .any(|curve| curve.zone.as_deref() == Some(zone.id.as_str()))
                    {
                        return Err(format!(
                            "thermal zone {:?} has no curve and no curve claims the zone",
                            zone.id
                        ));
                    }
                }
            }
        }
        for curve in &self.curves {
            if let Some(zone) = &curve.zone {
                if !zone_ids.contains(zone.as_str()) {
                    return Err(format!(
                        "fan curve {:?} references unknown zone {:?}",
                        curve.id, zone
                    ));
                }
            }
            for fan_id in &curve.fan_ids {
                if !fan_ids.contains(fan_id.as_str()) {
                    return Err(format!(
                        "fan curve {:?} references unknown fan {:?}",
                        curve.id, fan_id
                    ));
                }
            }
        }
        Ok(())
    }
}

/// One sensor reading supplied to the control engine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SensorReading {
    pub id: String,
    pub name: String,
    pub temp_c: f64,
    /// Seconds since this sensor was sampled.
    #[serde(default)]
    pub age_secs: f64,
}

/// One fan observation supplied to the control engine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FanReading {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub rpm: Option<u32>,
    #[serde(default)]
    pub duty_percent: Option<f64>,
    #[serde(default)]
    pub controllable: bool,
}

/// Current status of one fan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FanStatus {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub rpm: Option<u32>,
    #[serde(default)]
    pub duty_percent: Option<f64>,
    #[serde(default)]
    pub target_duty_percent: Option<f64>,
    #[serde(default)]
    pub controllable: bool,
    #[serde(default)]
    pub estimated_dba: Option<f64>,
}

/// Current status of one zone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ZoneStatus {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub temp_c: Option<f64>,
    pub state: ThermalState,
    pub limits: ThermalLimits,
    #[serde(default)]
    pub target_duty_percent: Option<f64>,
    #[serde(default)]
    pub estimated_dba: Option<f64>,
}

/// Controller/node-facing thermal snapshot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ThermalStatus {
    pub ok: bool,
    pub control_enabled: bool,
    pub policy_loaded: bool,
    #[serde(default)]
    pub sensors: Vec<SensorStatus>,
    #[serde(default)]
    pub fans: Vec<FanStatus>,
    #[serde(default)]
    pub zones: Vec<ZoneStatus>,
    #[serde(default)]
    pub estimated_dba: Option<f64>,
    #[serde(default)]
    pub shutdown_requested: bool,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub updated_at: f64,
}

/// One sensor in a thermal snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SensorStatus {
    pub id: String,
    pub name: String,
    pub temp_c: f64,
    #[serde(default)]
    pub age_secs: f64,
}

/// Why a fan command was produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FanReason {
    Curve,
    Hysteresis,
    NoiseCapped,
    CriticalOverride,
    FailSafe,
    Unchanged,
}

/// Target duty for one fan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FanCommand {
    pub fan_id: String,
    pub duty_percent: f64,
    pub reason: FanReason,
}

/// Output of one control-engine tick.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ThermalPlan {
    pub zones: Vec<ZoneStatus>,
    pub fans: Vec<FanCommand>,
    pub estimated_dba: Option<f64>,
    pub warnings: Vec<String>,
    pub shutdown_requested: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> ThermalLimits {
        ThermalLimits {
            safe_c: 80.0,
            warning_c: 88.0,
            critical_c: 95.0,
            shutdown_c: Some(105.0),
        }
    }

    #[test]
    fn limits_classify_temperature_states() {
        let limits = limits();
        assert_eq!(limits.state_for(50.0), ThermalState::Normal);
        assert_eq!(limits.state_for(88.0), ThermalState::Warning);
        assert_eq!(limits.state_for(95.0), ThermalState::Critical);
        assert_eq!(limits.state_for(105.0), ThermalState::Shutdown);
    }

    #[test]
    fn limits_reject_unordered_thresholds() {
        let bad = ThermalLimits {
            safe_c: 90.0,
            warning_c: 85.0,
            critical_c: 95.0,
            shutdown_c: None,
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn curve_validation_requires_ordered_points() {
        let curve = FanCurve {
            id: "cpu".into(),
            name: "CPU".into(),
            zone: Some("cpu".into()),
            fan_ids: vec![],
            sensor_ids: vec![],
            sensor_mode: SensorMode::Max,
            sensor_weights: BTreeMap::new(),
            points: vec![
                FanCurvePoint {
                    temp_c: 40.0,
                    duty_percent: 30.0,
                },
                FanCurvePoint {
                    temp_c: 30.0,
                    duty_percent: 40.0,
                },
            ],
            hysteresis_c: 2.0,
            ramp_rate_percent_per_sec: Some(10.0),
            min_duty_percent: 20.0,
            max_duty_percent: 100.0,
            critical_override: None,
            noise_cap: None,
        };
        assert!(curve.validate().is_err());
    }

    #[test]
    fn policy_rejects_unknown_fan_references() {
        let policy = ThermalPolicy {
            zones: vec![ThermalZone {
                id: "cpu".into(),
                name: "CPU".into(),
                sensor_ids: vec!["cpu_temp".into()],
                fan_ids: vec!["missing".into()],
                curve_id: Some("cpu".into()),
                limits: limits(),
            }],
            curves: vec![FanCurve {
                id: "cpu".into(),
                name: "CPU".into(),
                zone: Some("cpu".into()),
                fan_ids: vec![],
                sensor_ids: vec![],
                sensor_mode: SensorMode::Max,
                sensor_weights: BTreeMap::new(),
                points: vec![FanCurvePoint {
                    temp_c: 40.0,
                    duty_percent: 30.0,
                }],
                hysteresis_c: 2.0,
                ramp_rate_percent_per_sec: None,
                min_duty_percent: 20.0,
                max_duty_percent: 100.0,
                critical_override: None,
                noise_cap: None,
            }],
            fans: vec![],
            ..ThermalPolicy::default()
        };
        assert!(policy.validate().is_err());
    }
}
