// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Thermal zones, fan curves, anti-cycling control, and fan noise estimation.
//!
//! The engine is pure and side-effect free. [`hwmon`] provides the Linux sysfs
//! backend that reads temperatures/fan speeds and, only when explicitly asked,
//! writes PWM duty. Thermal safety always outranks acoustic comfort.

pub mod acoustics;
pub mod curve;
pub mod hwmon;
pub mod model;

pub use acoustics::{combine_dba, estimate_fan_dba, max_duty_for_dba};
pub use curve::{ThermalEngine, ThermalError};
pub use hwmon::{
    HwmonController, HwmonError, HwmonFan, HwmonInventory, HwmonSensor, DEFAULT_HWMON_ROOT,
};
pub use model::{
    CriticalAction, CriticalOverride, FanCommand, FanCurve, FanCurvePoint, FanReading, FanReason,
    FanSpec, FanStatus, NoiseCap, NoisePoint, SensorMode, SensorReading, SensorStatus,
    ThermalLimits, ThermalPlan, ThermalPolicy, ThermalState, ThermalStatus, ThermalZone,
    ZoneStatus,
};
