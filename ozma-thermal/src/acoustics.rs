// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Fan acoustic estimation.
//!
//! Manufacturer dBA figures are approximate and usually measured at one metre
//! in free air. Ozma uses them to keep fan noise below a configured comfort
//! threshold, never as a calibrated sound-pressure measurement. Thermal safety
//! always outranks the estimate.

use crate::model::{FanSpec, NoisePoint};

/// Lowest duty considered non-zero for acoustic interpolation.
const MIN_AUDIBLE_DUTY: f64 = 0.5;

/// Estimate one fan's dBA at a duty percentage.
///
/// Returns `None` when the spec has no acoustic information. A zero duty is
/// reported as `0.0` dBA (the fan is off), which is a convention rather than a
/// sound measurement.
pub fn estimate_fan_dba(spec: &FanSpec, duty_percent: f64) -> Option<f64> {
    if !duty_percent.is_finite() {
        return None;
    }
    let duty = duty_percent.clamp(0.0, 100.0);
    if duty <= 0.0 {
        return Some(0.0);
    }
    let duty = duty.max(MIN_AUDIBLE_DUTY);

    if !spec.noise_points.is_empty() {
        let rpm = rpm_for_duty(spec, duty)?;
        return interpolate_noise_points(&spec.noise_points, rpm);
    }

    let max_dba = spec.noise_dba?;
    let (min_rpm, max_rpm) = rpm_bounds(spec)?;
    if max_rpm > min_rpm {
        let rpm = min_rpm + (max_rpm - min_rpm) * duty / 100.0;
        let idle_dba = spec
            .noise_at_idle_dba
            .unwrap_or_else(|| (max_dba - 20.0).max(0.0));
        let fraction = ((rpm.max(1.0) / min_rpm.max(1.0)).ln()) / (max_rpm / min_rpm.max(1.0)).ln();
        return Some(clamp_dba(idle_dba + (max_dba - idle_dba) * fraction));
    }

    // No RPM range: use the fan law, 20*log10(duty), relative to full speed.
    Some(clamp_dba(max_dba + 20.0 * (duty / 100.0).log10()))
}

/// The highest duty whose estimated dBA stays at or below `limit_dba`.
///
/// Returns `None` when the fan has no acoustic model; callers must not pretend
/// a noise cap was enforced in that case.
pub fn max_duty_for_dba(spec: &FanSpec, limit_dba: f64) -> Option<f64> {
    if !limit_dba.is_finite() {
        return None;
    }
    let at_zero = estimate_fan_dba(spec, 0.0)?;
    if at_zero > limit_dba {
        return Some(0.0);
    }
    let at_full = estimate_fan_dba(spec, 100.0)?;
    if at_full <= limit_dba {
        return Some(100.0);
    }

    // Monotonic curves make a bounded binary search sufficient.
    let mut low = 0.0;
    let mut high = 100.0;
    for _ in 0..24 {
        let middle = (low + high) / 2.0;
        match estimate_fan_dba(spec, middle) {
            Some(dba) if dba <= limit_dba => low = middle,
            _ => high = middle,
        }
    }
    Some(low)
}

/// Combine independent fan sources into one estimated dBA level.
pub fn combine_dba(values: impl IntoIterator<Item = f64>) -> Option<f64> {
    let mut power = 0.0;
    let mut any = false;
    for value in values {
        if !value.is_finite() || value < 0.0 {
            continue;
        }
        any = true;
        power += 10f64.powf(value / 10.0);
    }
    any.then(|| 10.0 * power.log10())
}

fn rpm_bounds(spec: &FanSpec) -> Option<(f64, f64)> {
    match (spec.min_rpm, spec.max_rpm) {
        (Some(min), Some(max)) if min < max => Some((min as f64, max as f64)),
        _ => {
            let first = spec.noise_points.first()?;
            let last = spec.noise_points.last()?;
            (first.rpm < last.rpm).then_some((first.rpm, last.rpm))
        }
    }
}

fn rpm_for_duty(spec: &FanSpec, duty: f64) -> Option<f64> {
    let (min, max) = rpm_bounds(spec)?;
    Some(min + (max - min) * duty / 100.0)
}

fn interpolate_noise_points(points: &[NoisePoint], rpm: f64) -> Option<f64> {
    let first = points.first()?;
    let last = points.last()?;
    if rpm <= first.rpm {
        return Some(clamp_dba(first.dba));
    }
    if rpm >= last.rpm {
        return Some(clamp_dba(last.dba));
    }
    for pair in points.windows(2) {
        let [low, high] = pair else { continue };
        if rpm >= low.rpm && rpm <= high.rpm {
            let span = (high.rpm / low.rpm).ln();
            if span.abs() < f64::EPSILON {
                return Some(clamp_dba(low.dba));
            }
            let fraction = (rpm / low.rpm).ln() / span;
            return Some(clamp_dba(low.dba + (high.dba - low.dba) * fraction));
        }
    }
    Some(clamp_dba(last.dba))
}

fn clamp_dba(value: f64) -> f64 {
    value.clamp(0.0, 120.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> FanSpec {
        FanSpec {
            id: "case".into(),
            name: "Case fan".into(),
            count: 1,
            min_rpm: Some(400),
            max_rpm: Some(1600),
            noise_dba: Some(30.0),
            noise_at_idle_dba: Some(16.0),
            noise_points: vec![],
            location: None,
            controllable: true,
        }
    }

    #[test]
    fn zero_duty_is_silent() {
        assert_eq!(estimate_fan_dba(&spec(), 0.0), Some(0.0));
    }

    #[test]
    fn full_duty_matches_rated_noise() {
        assert!((estimate_fan_dba(&spec(), 100.0).unwrap() - 30.0).abs() < 0.01);
    }

    #[test]
    fn idle_duty_is_near_rated_idle_noise() {
        let dba = estimate_fan_dba(&spec(), 0.0).unwrap();
        assert_eq!(dba, 0.0);
    }

    #[test]
    fn noise_points_interpolate_on_rpm() {
        let fan = FanSpec {
            noise_points: vec![
                NoisePoint {
                    rpm: 500.0,
                    dba: 18.0,
                },
                NoisePoint {
                    rpm: 1500.0,
                    dba: 30.0,
                },
            ],
            ..spec()
        };
        let dba = estimate_fan_dba(&fan, 50.0).unwrap();
        assert!(dba > 18.0 && dba < 30.0, "{dba}");
        assert!((estimate_fan_dba(&fan, 100.0).unwrap() - 30.0).abs() < 0.01);
    }

    #[test]
    fn max_duty_for_dba_respects_the_limit() {
        let fan = spec();
        let limit = 24.0;
        let duty = max_duty_for_dba(&fan, limit).unwrap();
        assert!(duty > 0.0 && duty < 100.0);
        assert!(estimate_fan_dba(&fan, duty).unwrap() <= limit + 0.01);
        assert!(estimate_fan_dba(&fan, (duty + 5.0).min(100.0)).unwrap() > limit - 0.5);
    }

    #[test]
    fn combining_sources_adds_acoustic_power() {
        assert!((combine_dba([30.0]).unwrap() - 30.0).abs() < 0.01);
        assert!((combine_dba([30.0, 30.0]).unwrap() - 33.0103).abs() < 0.01);
        assert_eq!(combine_dba(std::iter::empty()), None);
    }

    #[test]
    fn fan_without_acoustic_model_has_no_estimate() {
        let fan = FanSpec {
            noise_dba: None,
            noise_at_idle_dba: None,
            noise_points: vec![],
            ..spec()
        };
        assert_eq!(estimate_fan_dba(&fan, 50.0), None);
        assert_eq!(max_duty_for_dba(&fan, 30.0), None);
    }
}
