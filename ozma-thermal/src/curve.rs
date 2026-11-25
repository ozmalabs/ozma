// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Fan-curve evaluation and anti-cycling state machine.
//!
//! The engine is pure: it reads sensor/fan snapshots supplied by the caller and
//! returns a [`ThermalPlan`]. It never touches hardware. Safety always outranks
//! acoustic comfort; the critical override bypasses every smoothing mechanism.

use std::collections::{BTreeMap, BTreeSet};

use crate::acoustics::{combine_dba, estimate_fan_dba};
use crate::model::{
    CriticalAction, FanCommand, FanCurve, FanReading, FanReason, FanSpec, SensorMode,
    SensorReading, ThermalPlan, ThermalPolicy, ThermalState, ThermalZone, ZoneStatus,
};

/// Duty changes smaller than this are treated as "unchanged".
const DUTY_EPSILON: f64 = 1e-9;

/// Largest tick delta used when applying a ramp limit.
const MAX_RAMP_DT_SECS: f64 = 60.0;

/// Errors raised while validating or running a thermal policy.
#[derive(Debug, thiserror::Error)]
pub enum ThermalError {
    #[error("invalid thermal policy: {0}")]
    InvalidPolicy(String),
}

/// Per-zone anti-cycling state.
///
/// `last_temp` is the representative temperature at the last command that was
/// not held back by hysteresis, dwell, or ramp limiting. Keeping that reference
/// (rather than every sampled temperature) prevents slow temperature drift from
/// silently defeating hysteresis and prevents a partially ramped command from
/// latching as the new hysteresis reference.
#[derive(Clone, Debug, Default)]
struct ZoneState {
    last_temp: Option<f64>,
    last_duty: Option<f64>,
    last_tick: Option<f64>,
    last_change: Option<f64>,
}

/// Stateful fan-curve engine.
#[derive(Debug)]
pub struct ThermalEngine {
    policy: ThermalPolicy,
    state: BTreeMap<String, ZoneState>,
}

impl ThermalEngine {
    /// Validate a policy and create an engine.
    pub fn new(policy: ThermalPolicy) -> Result<Self, ThermalError> {
        policy.validate().map_err(ThermalError::InvalidPolicy)?;
        Ok(Self {
            policy,
            state: BTreeMap::new(),
        })
    }

    /// The policy this engine was built from.
    pub fn policy(&self) -> &ThermalPolicy {
        &self.policy
    }

    /// Evaluate one control tick.
    ///
    /// `now_secs` is a monotonic timestamp in seconds. Sensor and fan readings
    /// are caller-supplied snapshots; no hardware access occurs here.
    pub fn tick(
        &mut self,
        now_secs: f64,
        sensors: &BTreeMap<String, SensorReading>,
        _fans: &BTreeMap<String, FanReading>,
    ) -> ThermalPlan {
        let policy = &self.policy;
        let states = &mut self.state;
        let mut plan = ThermalPlan::default();
        let mut fan_commands: BTreeMap<String, FanCommand> = BTreeMap::new();

        for zone in &policy.zones {
            let state = states.entry(zone.id.clone()).or_default();
            let curve = find_curve(policy, zone);

            let fresh = collect_fresh_sensors(policy, zone, sensors, &mut plan.warnings);

            if fresh.is_empty() {
                let previous = state.last_duty.unwrap_or(0.0);
                let duty = policy.failsafe_duty_percent.max(previous).clamp(0.0, 100.0);
                plan.warnings.push(format!(
                    "zone {:?}: no fresh sensors, using fail-safe duty {duty:.1}%",
                    zone.id
                ));

                record_command(state, now_secs, duty);
                let dba = zone_dba(policy, zone, duty);
                plan.zones.push(ZoneStatus {
                    id: zone.id.clone(),
                    name: zone.name.clone(),
                    temp_c: None,
                    state: ThermalState::Unknown,
                    limits: zone.limits.clone(),
                    target_duty_percent: Some(duty),
                    estimated_dba: dba,
                });
                for fan_id in unique_fan_ids(zone) {
                    merge_fan_command(&mut fan_commands, fan_id, duty, FanReason::FailSafe);
                }
                continue;
            }

            let Some(curve) = curve else {
                // `ThermalPolicy::validate` guarantees a curve exists. Stay safe
                // if a policy was mutated behind our back: never lower the fans.
                plan.warnings.push(format!(
                    "zone {:?}: no fan curve found, using fail-safe duty",
                    zone.id
                ));
                let duty = policy
                    .failsafe_duty_percent
                    .max(state.last_duty.unwrap_or(0.0))
                    .clamp(0.0, 100.0);
                // Without a curve, the hottest reading is the safest representative.
                let temp = fresh
                    .iter()
                    .map(|(_, temp)| *temp)
                    .fold(f64::NEG_INFINITY, f64::max);
                let state_label = zone.limits.state_for(temp);
                record_command(state, now_secs, duty);
                let dba = zone_dba(policy, zone, duty);
                plan.zones.push(ZoneStatus {
                    id: zone.id.clone(),
                    name: zone.name.clone(),
                    temp_c: Some(temp),
                    state: state_label,
                    limits: zone.limits.clone(),
                    target_duty_percent: Some(duty),
                    estimated_dba: dba,
                });
                for fan_id in unique_fan_ids(zone) {
                    merge_fan_command(&mut fan_commands, fan_id, duty, FanReason::FailSafe);
                }
                continue;
            };

            let temp = representative_temp(curve, &fresh, &mut plan.warnings, &zone.id);

            let critical_threshold = effective_critical_threshold(zone, curve);
            let mut shutdown = false;

            if let Some(shutdown_c) = zone.limits.shutdown_c {
                if temp >= shutdown_c {
                    shutdown = true;
                }
            }
            if let Some(override_) = &curve.critical_override {
                if override_.action == CriticalAction::Shutdown
                    && temp >= override_.shutdown_c.unwrap_or(override_.threshold_c)
                {
                    shutdown = true;
                }
            }

            let critical = temp >= critical_threshold;
            let mut state_label = zone.limits.state_for(temp);
            if critical {
                state_label = ThermalState::Critical;
            }
            if shutdown {
                state_label = ThermalState::Shutdown;
            }

            let (duty, reason) = if critical {
                plan.warnings.push(format!(
                    "zone {:?}: critical temperature {temp:.1}C, overriding to 100%",
                    zone.id
                ));
                if shutdown {
                    plan.shutdown_requested = true;
                    plan.warnings
                        .push(format!("zone {:?}: shutdown requested", zone.id));
                }
                if curve.critical_override.as_ref().map(|o| o.action)
                    == Some(CriticalAction::ThrottleAndMax)
                {
                    plan.warnings.push(format!(
                        "zone {:?}: throttle requested at critical temperature",
                        zone.id
                    ));
                }

                // Critical bypasses noise caps, max duty, ramp, and dwell.
                state.last_temp = Some(temp);
                record_command(state, now_secs, 100.0);
                (100.0, FanReason::CriticalOverride)
            } else {
                debug_assert!(!shutdown, "shutdown implies critical");

                let raw = curve_duty(curve, temp);
                let normal = state_label == ThermalState::Normal;

                let hysteresis = normal
                    && curve.hysteresis_c > 0.0
                    && matches!(
                        (state.last_temp, state.last_duty),
                        (Some(last_temp), Some(_)) if (temp - last_temp).abs() < curve.hysteresis_c
                    );

                if hysteresis {
                    let previous = state.last_duty.unwrap_or(raw);
                    record_command(state, now_secs, previous);
                    (previous, FanReason::Hysteresis)
                } else {
                    let mut candidate = raw;
                    let mut reason = FanReason::Curve;
                    let mut anti_hold = false;

                    // Noise comfort: only Normal/Warning; Critical already left.
                    if let Some(cap) = &curve.noise_cap {
                        let limit = match state_label {
                            ThermalState::Normal => Some(cap.comfort_dba),
                            ThermalState::Warning => Some(cap.warning_dba),
                            _ => None,
                        };
                        if let Some(limit) = limit {
                            match zone_dba(policy, zone, candidate) {
                                Some(level) if level > limit => {
                                    match zone_noise_duty(policy, zone, limit) {
                                        Some(capped) if capped < candidate => {
                                            candidate = capped;
                                            reason = FanReason::NoiseCapped;
                                        }
                                        Some(_) => {}
                                        None => plan.warnings.push(format!(
                                            "zone {:?}: noise cap cannot be enforced, no fan has an acoustic model",
                                            zone.id
                                        )),
                                    }
                                }
                                Some(_) => {}
                                None => plan.warnings.push(format!(
                                    "zone {:?}: noise cap cannot be enforced, no fan has an acoustic model",
                                    zone.id
                                )),
                            }
                        }
                    }

                    // Dwell: hold a recent command unless the zone is warning.
                    if normal {
                        if let Some(previous) = state.last_duty {
                            let wants_change = (candidate - previous).abs() > DUTY_EPSILON;
                            if wants_change
                                && !dwell_elapsed(state, now_secs, policy.min_dwell_secs)
                            {
                                candidate = previous;
                                reason = FanReason::Unchanged;
                                anti_hold = true;
                            }
                        }
                    }

                    // Ramp: limit how far the command may move this tick.
                    if !anti_hold {
                        if let Some(ramp) = curve.ramp_rate_percent_per_sec {
                            if let (Some(previous), Some(last_tick)) =
                                (state.last_duty, state.last_tick)
                            {
                                let dt = (now_secs - last_tick).clamp(0.0, MAX_RAMP_DT_SECS);
                                if dt > 0.0 && now_secs.is_finite() && last_tick.is_finite() {
                                    let max_change = ramp * dt;
                                    let delta = candidate - previous;
                                    if delta > max_change {
                                        candidate = previous + max_change;
                                        anti_hold = true;
                                    } else if delta < -max_change {
                                        candidate = previous - max_change;
                                        anti_hold = true;
                                    }
                                }
                            }
                        }
                    }

                    if !anti_hold {
                        state.last_temp = Some(temp);
                    }
                    record_command(state, now_secs, candidate);
                    (candidate, reason)
                }
            };

            let dba = zone_dba(policy, zone, duty);
            plan.zones.push(ZoneStatus {
                id: zone.id.clone(),
                name: zone.name.clone(),
                temp_c: Some(temp),
                state: state_label,
                limits: zone.limits.clone(),
                target_duty_percent: Some(duty),
                estimated_dba: dba,
            });
            for fan_id in unique_fan_ids(zone) {
                merge_fan_command(&mut fan_commands, fan_id, duty, reason);
            }
        }

        plan.fans = fan_commands.into_values().collect();
        plan.estimated_dba = plan_dba(policy, &plan.fans);
        plan
    }
}

fn find_curve<'a>(policy: &'a ThermalPolicy, zone: &ThermalZone) -> Option<&'a FanCurve> {
    if let Some(curve_id) = &zone.curve_id {
        return policy.curves.iter().find(|curve| curve.id == *curve_id);
    }
    policy
        .curves
        .iter()
        .find(|curve| curve.zone.as_deref() == Some(zone.id.as_str()))
}

fn find_spec<'a>(policy: &'a ThermalPolicy, fan_id: &str) -> Option<&'a FanSpec> {
    policy.fans.iter().find(|spec| spec.id == fan_id)
}

fn collect_fresh_sensors<'a>(
    policy: &ThermalPolicy,
    zone: &'a ThermalZone,
    sensors: &BTreeMap<String, SensorReading>,
    warnings: &mut Vec<String>,
) -> Vec<(&'a str, f64)> {
    let mut fresh = Vec::new();
    for sensor_id in &zone.sensor_ids {
        let Some(reading) = sensors.get(sensor_id) else {
            warnings.push(format!(
                "zone {:?}: sensor {:?} is missing",
                zone.id, sensor_id
            ));
            continue;
        };
        if !reading.temp_c.is_finite() {
            warnings.push(format!(
                "zone {:?}: sensor {:?} reported a non-finite temperature",
                zone.id, sensor_id
            ));
            continue;
        }
        if !reading.age_secs.is_finite() || reading.age_secs > policy.max_sensor_age_secs {
            warnings.push(format!(
                "zone {:?}: sensor {:?} is stale (age {:.1}s, limit {:.1}s)",
                zone.id, sensor_id, reading.age_secs, policy.max_sensor_age_secs
            ));
            continue;
        }
        fresh.push((sensor_id.as_str(), reading.temp_c));
    }
    fresh
}

fn representative_temp(
    curve: &FanCurve,
    readings: &[(&str, f64)],
    warnings: &mut Vec<String>,
    zone_id: &str,
) -> f64 {
    let average =
        readings.iter().map(|(_, temp)| *temp).sum::<f64>() / readings.len().max(1) as f64;

    match curve.sensor_mode {
        SensorMode::Max => readings
            .iter()
            .map(|(_, temp)| *temp)
            .fold(f64::NEG_INFINITY, f64::max),
        SensorMode::Average => average,
        SensorMode::Weighted => {
            let mut weighted_sum = 0.0;
            let mut total_weight = 0.0;
            for (sensor_id, temp) in readings {
                if let Some(weight) = curve.sensor_weights.get(*sensor_id) {
                    if weight.is_finite() && *weight > 0.0 {
                        weighted_sum += weight * temp;
                        total_weight += weight;
                    }
                }
            }
            if total_weight > 0.0 {
                let weighted = weighted_sum / total_weight;
                if weighted.is_finite() {
                    return weighted;
                }
            }
            warnings.push(format!(
                "zone {zone_id:?}: weighted sensor mode has no usable weights, falling back to average"
            ));
            average
        }
    }
}

fn effective_critical_threshold(zone: &ThermalZone, curve: &FanCurve) -> f64 {
    let mut threshold = zone.limits.critical_c;
    if let Some(override_) = &curve.critical_override {
        threshold = threshold.min(override_.threshold_c);
        if let Some(shutdown_c) = override_.shutdown_c {
            threshold = threshold.min(shutdown_c);
        }
    }
    threshold
}

fn curve_duty(curve: &FanCurve, temp_c: f64) -> f64 {
    let points = &curve.points;
    let raw = match points.as_slice() {
        [] => 0.0,
        [only] => only.duty_percent,
        [first, .., last] => {
            if temp_c <= first.temp_c {
                first.duty_percent
            } else if temp_c >= last.temp_c {
                last.duty_percent
            } else {
                let mut interpolated = last.duty_percent;
                for pair in points.windows(2) {
                    let [low, high] = pair else { continue };
                    if temp_c >= low.temp_c && temp_c <= high.temp_c {
                        let span = high.temp_c - low.temp_c;
                        if span.abs() <= f64::EPSILON {
                            interpolated = low.duty_percent;
                        } else {
                            let fraction = (temp_c - low.temp_c) / span;
                            interpolated = low.duty_percent
                                + (high.duty_percent - low.duty_percent) * fraction;
                        }
                        break;
                    }
                }
                interpolated
            }
        }
    };

    let low = curve.min_duty_percent.clamp(0.0, 100.0);
    let high = curve.max_duty_percent.clamp(0.0, 100.0);
    raw.clamp(low.min(high), low.max(high)).clamp(0.0, 100.0)
}

fn zone_dba(policy: &ThermalPolicy, zone: &ThermalZone, duty: f64) -> Option<f64> {
    combine_dba(unique_fan_ids(zone).into_iter().filter_map(|fan_id| {
        let spec = find_spec(policy, fan_id)?;
        let single = estimate_fan_dba(spec, duty)?;
        Some(with_count(single, spec.count))
    }))
}

fn zone_noise_duty(policy: &ThermalPolicy, zone: &ThermalZone, limit_dba: f64) -> Option<f64> {
    // Use the zone-level estimate so `FanSpec::count` and multiple fans are
    // included in the acoustic sum, not just one fan at a time.
    let at_full = zone_dba(policy, zone, 100.0)?;
    if at_full <= limit_dba {
        return Some(100.0);
    }

    let mut low = 0.0;
    let mut high = 100.0;
    for _ in 0..24 {
        let middle = (low + high) / 2.0;
        match zone_dba(policy, zone, middle) {
            Some(dba) if dba <= limit_dba => low = middle,
            _ => high = middle,
        }
    }
    Some(low)
}

fn plan_dba(policy: &ThermalPolicy, commands: &[FanCommand]) -> Option<f64> {
    combine_dba(commands.iter().filter_map(|command| {
        let spec = find_spec(policy, &command.fan_id)?;
        let single = estimate_fan_dba(spec, command.duty_percent)?;
        Some(with_count(single, spec.count))
    }))
}

/// Combining `count` identical sources is equivalent to adding `10 log10(count)`
/// to a single source's level.
fn with_count(dba: f64, count: u32) -> f64 {
    if count > 1 {
        dba + 10.0 * f64::from(count).log10()
    } else {
        dba
    }
}

fn unique_fan_ids(zone: &ThermalZone) -> BTreeSet<&str> {
    zone.fan_ids.iter().map(String::as_str).collect()
}

fn dwell_elapsed(state: &ZoneState, now_secs: f64, min_dwell_secs: f64) -> bool {
    match state.last_change {
        Some(last_change) if now_secs.is_finite() && last_change.is_finite() => {
            now_secs - last_change >= min_dwell_secs
        }
        _ => true,
    }
}

fn record_command(state: &mut ZoneState, now_secs: f64, duty: f64) {
    let duty = duty.clamp(0.0, 100.0);
    let changed = state
        .last_duty
        .is_none_or(|previous| (previous - duty).abs() > DUTY_EPSILON);
    if changed && now_secs.is_finite() {
        state.last_change = Some(now_secs);
    }
    state.last_duty = Some(duty);
    if now_secs.is_finite() {
        state.last_tick = Some(now_secs);
    }
}

fn reason_rank(reason: FanReason) -> u8 {
    match reason {
        FanReason::CriticalOverride => 5,
        FanReason::FailSafe => 4,
        FanReason::NoiseCapped => 3,
        FanReason::Hysteresis => 2,
        FanReason::Curve => 1,
        FanReason::Unchanged => 0,
    }
}

fn merge_fan_command(
    commands: &mut BTreeMap<String, FanCommand>,
    fan_id: &str,
    duty: f64,
    reason: FanReason,
) {
    let duty = duty.clamp(0.0, 100.0);
    match commands.get_mut(fan_id) {
        Some(existing) => {
            if duty > existing.duty_percent + DUTY_EPSILON {
                existing.duty_percent = duty;
                existing.reason = reason;
            } else if (duty - existing.duty_percent).abs() <= DUTY_EPSILON
                && reason_rank(reason) > reason_rank(existing.reason)
            {
                existing.reason = reason;
            }
        }
        None => {
            commands.insert(
                fan_id.to_owned(),
                FanCommand {
                    fan_id: fan_id.to_owned(),
                    duty_percent: duty,
                    reason,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CriticalOverride, FanCurvePoint, NoiseCap, ThermalLimits};

    fn limits() -> ThermalLimits {
        ThermalLimits {
            safe_c: 80.0,
            warning_c: 88.0,
            critical_c: 95.0,
            shutdown_c: None,
        }
    }

    fn fan(id: &str, dba: Option<f64>, count: u32) -> FanSpec {
        FanSpec {
            id: id.to_owned(),
            name: id.to_owned(),
            count,
            min_rpm: Some(400),
            max_rpm: Some(1600),
            noise_dba: dba,
            noise_at_idle_dba: dba.map(|_| 16.0),
            noise_points: Vec::new(),
            location: None,
            controllable: true,
        }
    }

    fn curve_spec(id: &str, zone: &str, points: &[(f64, f64)]) -> FanCurve {
        FanCurve {
            id: id.to_owned(),
            name: id.to_owned(),
            zone: Some(zone.to_owned()),
            fan_ids: Vec::new(),
            sensor_ids: Vec::new(),
            sensor_mode: SensorMode::Max,
            sensor_weights: BTreeMap::new(),
            points: points
                .iter()
                .map(|(temp_c, duty_percent)| FanCurvePoint {
                    temp_c: *temp_c,
                    duty_percent: *duty_percent,
                })
                .collect(),
            hysteresis_c: 0.0,
            ramp_rate_percent_per_sec: None,
            min_duty_percent: 0.0,
            max_duty_percent: 100.0,
            critical_override: None,
            noise_cap: None,
        }
    }

    fn zone(
        id: &str,
        sensors: &[&str],
        fans: &[&str],
        curve_id: &str,
        limits: ThermalLimits,
    ) -> ThermalZone {
        ThermalZone {
            id: id.to_owned(),
            name: id.to_owned(),
            sensor_ids: sensors.iter().map(|sensor| (*sensor).to_owned()).collect(),
            fan_ids: fans.iter().map(|fan| (*fan).to_owned()).collect(),
            curve_id: Some(curve_id.to_owned()),
            limits,
        }
    }

    fn policy(zones: Vec<ThermalZone>, curves: Vec<FanCurve>, fans: Vec<FanSpec>) -> ThermalPolicy {
        ThermalPolicy {
            zones,
            curves,
            fans,
            poll_interval_secs: 5,
            min_dwell_secs: 0.0,
            max_sensor_age_secs: 15.0,
            failsafe_duty_percent: 70.0,
        }
    }

    fn single(curve: FanCurve, fan_specs: Vec<FanSpec>, limits: ThermalLimits) -> ThermalPolicy {
        let fan_ids: Vec<&str> = fan_specs.iter().map(|spec| spec.id.as_str()).collect();
        let zone = zone("zone", &["sensor"], &fan_ids, &curve.id, limits);
        policy(vec![zone], vec![curve], fan_specs)
    }

    fn readings(items: &[(&str, f64)]) -> BTreeMap<String, SensorReading> {
        items
            .iter()
            .map(|(id, temp_c)| {
                (
                    (*id).to_owned(),
                    SensorReading {
                        id: (*id).to_owned(),
                        name: (*id).to_owned(),
                        temp_c: *temp_c,
                        age_secs: 0.0,
                    },
                )
            })
            .collect()
    }

    fn stale_readings(items: &[(&str, f64, f64)]) -> BTreeMap<String, SensorReading> {
        items
            .iter()
            .map(|(id, temp_c, age_secs)| {
                (
                    (*id).to_owned(),
                    SensorReading {
                        id: (*id).to_owned(),
                        name: (*id).to_owned(),
                        temp_c: *temp_c,
                        age_secs: *age_secs,
                    },
                )
            })
            .collect()
    }

    fn duty(plan: &ThermalPlan) -> f64 {
        plan.zones[0]
            .target_duty_percent
            .expect("zone should have a target duty")
    }

    fn no_fans() -> BTreeMap<String, FanReading> {
        BTreeMap::new()
    }

    #[test]
    fn curve_interpolates_and_clamps() {
        let curve = curve_spec(
            "curve",
            "zone",
            &[(40.0, 20.0), (60.0, 60.0), (80.0, 100.0)],
        );
        let mut engine =
            ThermalEngine::new(single(curve, vec![fan("fan", None, 1)], limits())).unwrap();

        let plan = engine.tick(0.0, &readings(&[("sensor", 50.0)]), &no_fans());
        assert!((duty(&plan) - 40.0).abs() < 1e-6);

        let plan = engine.tick(1.0, &readings(&[("sensor", 30.0)]), &no_fans());
        assert!((duty(&plan) - 20.0).abs() < 1e-6);

        let plan = engine.tick(2.0, &readings(&[("sensor", 90.0)]), &no_fans());
        assert!((duty(&plan) - 100.0).abs() < 1e-6);

        let mut bounded = curve_spec("bounded", "zone", &[(40.0, 10.0), (60.0, 100.0)]);
        bounded.min_duty_percent = 30.0;
        bounded.max_duty_percent = 70.0;
        let mut engine =
            ThermalEngine::new(single(bounded, vec![fan("fan", None, 1)], limits())).unwrap();
        let plan = engine.tick(0.0, &readings(&[("sensor", 20.0)]), &no_fans());
        assert!((duty(&plan) - 30.0).abs() < 1e-6);
        let plan = engine.tick(1.0, &readings(&[("sensor", 80.0)]), &no_fans());
        assert!((duty(&plan) - 70.0).abs() < 1e-6);
    }

    fn mode_engine(mode: SensorMode, weights: BTreeMap<String, f64>) -> ThermalEngine {
        let mut curve = curve_spec("curve", "zone", &[(0.0, 0.0), (100.0, 100.0)]);
        curve.sensor_mode = mode;
        curve.sensor_weights = weights;
        let zone = zone("zone", &["s1", "s2", "s3"], &["fan"], "curve", limits());
        ThermalEngine::new(policy(vec![zone], vec![curve], vec![fan("fan", None, 1)])).unwrap()
    }

    #[test]
    fn sensor_modes_combine_readings() {
        let values = readings(&[("s1", 40.0), ("s2", 60.0), ("s3", 80.0)]);

        let plan = mode_engine(SensorMode::Max, BTreeMap::new()).tick(0.0, &values, &no_fans());
        assert!((plan.zones[0].temp_c.unwrap() - 80.0).abs() < 1e-6);

        let plan = mode_engine(SensorMode::Average, BTreeMap::new()).tick(0.0, &values, &no_fans());
        assert!((plan.zones[0].temp_c.unwrap() - 60.0).abs() < 1e-6);

        let weights = BTreeMap::from([("s1".to_owned(), 3.0), ("s2".to_owned(), 1.0)]);
        let plan = mode_engine(SensorMode::Weighted, weights).tick(0.0, &values, &no_fans());
        assert!((plan.zones[0].temp_c.unwrap() - 45.0).abs() < 1e-6);

        let unusable = BTreeMap::from([
            ("s1".to_owned(), 0.0),
            ("s2".to_owned(), -1.0),
            ("s3".to_owned(), f64::NAN),
        ]);
        let mut engine = mode_engine(SensorMode::Weighted, unusable);
        let plan = engine.tick(0.0, &values, &no_fans());
        assert!((plan.zones[0].temp_c.unwrap() - 60.0).abs() < 1e-6);
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("weights")));
    }

    #[test]
    fn missing_or_stale_sensors_are_fail_safe_and_never_lower_fans() {
        let curve = curve_spec("curve", "zone", &[(40.0, 20.0), (80.0, 100.0)]);
        let mut engine =
            ThermalEngine::new(single(curve, vec![fan("fan", None, 1)], limits())).unwrap();

        let plan = engine.tick(0.0, &readings(&[("sensor", 80.0)]), &no_fans());
        assert!((duty(&plan) - 100.0).abs() < 1e-6);

        let plan = engine.tick(10.0, &BTreeMap::new(), &no_fans());
        assert!((duty(&plan) - 100.0).abs() < 1e-6);
        assert_eq!(plan.zones[0].temp_c, None);
        assert_eq!(plan.zones[0].state, ThermalState::Unknown);
        assert_eq!(plan.fans[0].reason, FanReason::FailSafe);
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("fail-safe")));

        let curve = curve_spec("curve", "zone", &[(40.0, 20.0), (80.0, 100.0)]);
        let mut engine =
            ThermalEngine::new(single(curve, vec![fan("fan", None, 1)], limits())).unwrap();
        let plan = engine.tick(0.0, &BTreeMap::new(), &no_fans());
        assert!((duty(&plan) - 70.0).abs() < 1e-6);

        let plan = engine.tick(1.0, &stale_readings(&[("sensor", 80.0, 20.0)]), &no_fans());
        assert!((duty(&plan) - 70.0).abs() < 1e-6);
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("stale")));
    }

    #[test]
    fn hysteresis_holds_small_temperature_changes() {
        let mut curve = curve_spec(
            "curve",
            "zone",
            &[(40.0, 20.0), (60.0, 60.0), (80.0, 100.0)],
        );
        curve.hysteresis_c = 2.0;
        let mut engine =
            ThermalEngine::new(single(curve, vec![fan("fan", None, 1)], limits())).unwrap();

        let plan = engine.tick(0.0, &readings(&[("sensor", 50.0)]), &no_fans());
        assert!((duty(&plan) - 40.0).abs() < 1e-6);

        let plan = engine.tick(5.0, &readings(&[("sensor", 51.0)]), &no_fans());
        assert!((duty(&plan) - 40.0).abs() < 1e-6);
        assert_eq!(plan.fans[0].reason, FanReason::Hysteresis);

        let plan = engine.tick(10.0, &readings(&[("sensor", 53.0)]), &no_fans());
        assert!(duty(&plan) > 40.0);
    }

    #[test]
    fn dwell_prevents_rapid_cycling() {
        let curve = curve_spec("curve", "zone", &[(40.0, 20.0), (80.0, 100.0)]);
        let mut policy = single(curve, vec![fan("fan", None, 1)], limits());
        policy.min_dwell_secs = 30.0;
        let mut engine = ThermalEngine::new(policy).unwrap();

        let plan = engine.tick(0.0, &readings(&[("sensor", 40.0)]), &no_fans());
        assert!((duty(&plan) - 20.0).abs() < 1e-6);

        let plan = engine.tick(1.0, &readings(&[("sensor", 80.0)]), &no_fans());
        assert!((duty(&plan) - 20.0).abs() < 1e-6);
        assert_eq!(plan.fans[0].reason, FanReason::Unchanged);

        let plan = engine.tick(31.0, &readings(&[("sensor", 80.0)]), &no_fans());
        assert!((duty(&plan) - 100.0).abs() < 1e-6);
    }

    #[test]
    fn ramp_limits_changes_but_critical_bypasses_it() {
        let mut curve = curve_spec("curve", "zone", &[(40.0, 20.0), (80.0, 100.0)]);
        curve.ramp_rate_percent_per_sec = Some(10.0);
        let mut engine =
            ThermalEngine::new(single(curve, vec![fan("fan", None, 1)], limits())).unwrap();

        let plan = engine.tick(0.0, &readings(&[("sensor", 40.0)]), &no_fans());
        assert!((duty(&plan) - 20.0).abs() < 1e-6);

        let plan = engine.tick(1.0, &readings(&[("sensor", 75.0)]), &no_fans());
        assert!((duty(&plan) - 30.0).abs() < 1e-6);

        let plan = engine.tick(2.0, &readings(&[("sensor", 96.0)]), &no_fans());
        assert!((duty(&plan) - 100.0).abs() < 1e-6);
        assert_eq!(plan.fans[0].reason, FanReason::CriticalOverride);
    }

    #[test]
    fn critical_override_maxes_fans_and_requests_shutdown() {
        let mut curve = curve_spec("curve", "zone", &[(40.0, 20.0), (90.0, 100.0)]);
        curve.critical_override = Some(CriticalOverride {
            threshold_c: 90.0,
            shutdown_c: Some(100.0),
            action: CriticalAction::Shutdown,
        });
        let mut engine = ThermalEngine::new(single(
            curve.clone(),
            vec![fan("fan", None, 1)],
            ThermalLimits {
                shutdown_c: Some(105.0),
                ..limits()
            },
        ))
        .unwrap();

        let plan = engine.tick(0.0, &readings(&[("sensor", 92.0)]), &no_fans());
        assert!((duty(&plan) - 100.0).abs() < 1e-6);
        assert_eq!(plan.fans[0].reason, FanReason::CriticalOverride);
        assert_eq!(plan.zones[0].state, ThermalState::Critical);
        assert!(!plan.shutdown_requested);

        let plan = engine.tick(5.0, &readings(&[("sensor", 100.0)]), &no_fans());
        assert!((duty(&plan) - 100.0).abs() < 1e-6);
        assert!(plan.shutdown_requested);
        assert_eq!(plan.zones[0].state, ThermalState::Shutdown);
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("shutdown")));

        let mut throttle = curve.clone();
        throttle.critical_override = Some(CriticalOverride {
            threshold_c: 90.0,
            shutdown_c: None,
            action: CriticalAction::ThrottleAndMax,
        });
        let mut engine =
            ThermalEngine::new(single(throttle, vec![fan("fan", None, 1)], limits())).unwrap();
        let plan = engine.tick(0.0, &readings(&[("sensor", 92.0)]), &no_fans());
        assert!((duty(&plan) - 100.0).abs() < 1e-6);
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("throttle requested")));
    }

    #[test]
    fn noise_cap_applies_in_normal_relaxes_at_warning_and_ignores_critical() {
        let mut curve = curve_spec("curve", "zone", &[(20.0, 100.0), (90.0, 100.0)]);
        curve.noise_cap = Some(NoiseCap {
            comfort_dba: 25.0,
            warning_dba: 35.0,
        });
        let mut engine =
            ThermalEngine::new(single(curve, vec![fan("fan", Some(40.0), 1)], limits())).unwrap();

        let normal = engine.tick(0.0, &readings(&[("sensor", 30.0)]), &no_fans());
        assert!(duty(&normal) < 100.0);
        assert_eq!(normal.fans[0].reason, FanReason::NoiseCapped);
        assert!(normal.zones[0].estimated_dba.unwrap() <= 25.0 + 1e-6);

        let warning = engine.tick(10.0, &readings(&[("sensor", 89.0)]), &no_fans());
        assert!(duty(&warning) > duty(&normal));
        assert!(duty(&warning) < 100.0);
        assert_eq!(warning.fans[0].reason, FanReason::NoiseCapped);

        let critical = engine.tick(20.0, &readings(&[("sensor", 96.0)]), &no_fans());
        assert!((duty(&critical) - 100.0).abs() < 1e-6);
        assert_eq!(critical.fans[0].reason, FanReason::CriticalOverride);
    }

    #[test]
    fn unenforceable_noise_cap_is_reported_honestly() {
        let mut curve = curve_spec("curve", "zone", &[(20.0, 100.0), (90.0, 100.0)]);
        curve.noise_cap = Some(NoiseCap {
            comfort_dba: 25.0,
            warning_dba: 35.0,
        });
        let mut engine =
            ThermalEngine::new(single(curve, vec![fan("fan", None, 1)], limits())).unwrap();

        let plan = engine.tick(0.0, &readings(&[("sensor", 30.0)]), &no_fans());
        assert!((duty(&plan) - 100.0).abs() < 1e-6);
        assert_eq!(plan.fans[0].reason, FanReason::Curve);
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("acoustic model")));
    }

    #[test]
    fn shared_fan_takes_maximum_zone_duty() {
        let curve_a = curve_spec("curve-a", "zone-a", &[(40.0, 20.0), (80.0, 100.0)]);
        let curve_b = curve_spec("curve-b", "zone-b", &[(40.0, 20.0), (80.0, 100.0)]);
        let zone_a = zone("zone-a", &["sa"], &["shared"], "curve-a", limits());
        let zone_b = zone("zone-b", &["sb"], &["shared"], "curve-b", limits());
        let mut engine = ThermalEngine::new(policy(
            vec![zone_a, zone_b],
            vec![curve_a, curve_b],
            vec![fan("shared", None, 1)],
        ))
        .unwrap();

        let plan = engine.tick(0.0, &readings(&[("sa", 40.0), ("sb", 60.0)]), &no_fans());
        assert_eq!(plan.fans.len(), 1);
        assert_eq!(plan.fans[0].fan_id, "shared");
        assert!((plan.fans[0].duty_percent - 60.0).abs() < 1e-6);
        assert_eq!(plan.fans[0].reason, FanReason::Curve);
    }

    #[test]
    fn combined_dba_accounts_for_fan_count() {
        let curve = curve_spec("curve", "zone", &[(20.0, 100.0), (90.0, 100.0)]);
        let mut engine =
            ThermalEngine::new(single(curve, vec![fan("pair", Some(30.0), 2)], limits())).unwrap();

        let plan = engine.tick(0.0, &readings(&[("sensor", 30.0)]), &no_fans());
        assert!((duty(&plan) - 100.0).abs() < 1e-6);
        let expected = 30.0 + 10.0 * 2f64.log10();
        assert!((plan.estimated_dba.unwrap() - expected).abs() < 1e-6);
        assert!((plan.zones[0].estimated_dba.unwrap() - expected).abs() < 1e-6);
    }

    #[test]
    fn noise_cap_accounts_for_the_whole_fan_group() {
        let mut curve = curve_spec("curve", "zone", &[(20.0, 100.0), (90.0, 100.0)]);
        curve.noise_cap = Some(NoiseCap {
            comfort_dba: 29.0,
            warning_dba: 40.0,
        });
        let mut engine =
            ThermalEngine::new(single(curve, vec![fan("pair", Some(30.0), 2)], limits())).unwrap();

        let plan = engine.tick(0.0, &readings(&[("sensor", 30.0)]), &no_fans());
        assert!(
            duty(&plan) < 100.0,
            "two 30 dBA fans must not run at full speed"
        );
        assert!(
            plan.zones[0].estimated_dba.unwrap() <= 29.0 + 1e-6,
            "zone dBA must respect the cap including fan count"
        );
    }
}
