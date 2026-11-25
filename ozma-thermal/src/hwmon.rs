// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Linux hwmon sensor and PWM fan backend.
//!
//! Discovery follows the sysfs layout documented in
//! [`Documentation/hwmon/sysfs-interface`](https://docs.kernel.org/hwmon/sysfs-interface.html):
//!
//! * `tempN_input` is millidegrees Celsius, optionally labelled by `tempN_label`.
//! * `fanN_input` is RPM, optionally labelled by `fanN_label`.
//! * `pwmN` is an 8-bit duty value and `pwmN_enable` selects the chip's control
//!   mode; `pwmN_label` labels PWM-only fans.
//!
//! The inventory is read-only. [`HwmonController`] is the only type that writes
//! to sysfs, and it always remembers the original `pwmN_enable` value so that
//! [`HwmonController::restore`] (also called from `Drop`) can hand the fans back
//! to firmware/automatic control.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::model::{FanReading, SensorReading};

/// Default sysfs hwmon root.
pub const DEFAULT_HWMON_ROOT: &str = "/sys/class/hwmon";

/// Errors raised by the hwmon backend.
#[derive(Debug, thiserror::Error)]
pub enum HwmonError {
    #[error("hwmon I/O failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("hwmon fan {0:?} has no PWM control")]
    NotControllable(String),
    #[error("hwmon fan {0:?} has no stored automatic mode to restore")]
    NoRestoreState(String),
    #[error("hwmon fan {0:?} is not present in the inventory")]
    UnknownFan(String),
    #[error("hwmon duty {0} is not finite")]
    InvalidDuty(f64),
}

/// One temperature sensor exposed by hwmon.
#[derive(Clone, Debug)]
pub struct HwmonSensor {
    pub id: String,
    pub name: String,
    pub chip: String,
    pub index: u32,
    pub input_path: PathBuf,
}

/// One fan exposed by hwmon.
#[derive(Clone, Debug)]
pub struct HwmonFan {
    pub id: String,
    pub name: String,
    pub chip: String,
    pub index: u32,
    pub rpm_path: Option<PathBuf>,
    pub pwm_path: Option<PathBuf>,
    pub pwm_enable_path: Option<PathBuf>,
    /// True only when both `pwmN` and `pwmN_enable` are present.
    pub controllable: bool,
}

/// Discovered hwmon sensors and fans.
#[derive(Clone, Debug, Default)]
pub struct HwmonInventory {
    pub sensors: Vec<HwmonSensor>,
    pub fans: Vec<HwmonFan>,
}

impl HwmonInventory {
    /// Discover sensors and fans below a sysfs hwmon root.
    ///
    /// A missing root is treated as an empty inventory. Individual unreadable
    /// entries and malformed names are skipped rather than failing discovery.
    /// Duplicate chip names are disambiguated with the hwmon directory name so
    /// IDs stay unique across chips (for example
    /// `hwmon:amdgpu@hwmon3:pwm1`).
    pub fn discover(root: &Path) -> Result<Self, HwmonError> {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(source) if source.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(HwmonError::Io {
                    path: root.to_path_buf(),
                    source,
                })
            }
        };

        let mut chips: Vec<ChipDir> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let dir_name = entry.file_name().to_string_lossy().into_owned();
                if !dir_name.starts_with("hwmon") || !path.is_dir() {
                    return None;
                }
                let chip = read_trimmed(&path.join("name")).unwrap_or_else(|| dir_name.clone());
                Some(ChipDir {
                    path,
                    dir_name,
                    chip,
                })
            })
            .collect();
        chips.sort_by(|left, right| left.dir_name.cmp(&right.dir_name));

        let mut chip_counts: HashMap<&str, usize> = HashMap::new();
        for chip in &chips {
            *chip_counts.entry(chip.chip.as_str()).or_default() += 1;
        }

        let mut sensors = Vec::new();
        let mut fans = Vec::new();
        for chip in &chips {
            let duplicate = chip_counts
                .get(chip.chip.as_str())
                .copied()
                .unwrap_or_default()
                > 1;
            let prefix = if duplicate {
                format!("hwmon:{}@{}", chip.chip, chip.dir_name)
            } else {
                format!("hwmon:{}", chip.chip)
            };

            sensors.extend(discover_sensors(chip, &prefix));
            fans.extend(discover_fans(chip, &prefix));
        }

        sensors.sort_by(|left, right| {
            left.chip
                .cmp(&right.chip)
                .then(left.index.cmp(&right.index))
                .then(left.id.cmp(&right.id))
        });
        fans.sort_by(|left, right| {
            left.chip
                .cmp(&right.chip)
                .then(left.index.cmp(&right.index))
                .then(left.id.cmp(&right.id))
        });

        Ok(Self { sensors, fans })
    }

    /// Read every currently available temperature sensor.
    ///
    /// Values outside the plausible `-40..=150 C` range, malformed values, and
    /// unreadable files are omitted. `age_secs` is always `0.0`; callers own
    /// staleness tracking.
    pub fn read_sensors(&self) -> Result<Vec<SensorReading>, HwmonError> {
        let mut readings = Vec::new();
        for sensor in &self.sensors {
            if let Some(temp_c) = read_temperature(&sensor.input_path) {
                readings.push(SensorReading {
                    id: sensor.id.clone(),
                    name: sensor.name.clone(),
                    temp_c,
                    age_secs: 0.0,
                });
            }
        }
        Ok(readings)
    }

    /// Read every discovered fan.
    ///
    /// An unreadable or implausible RPM/PWM value becomes `None` for that
    /// field; the fan itself is still reported so its control state remains
    /// visible. `age_secs` does not exist on [`FanReading`]; callers own
    /// staleness tracking.
    pub fn read_fans(&self) -> Result<Vec<FanReading>, HwmonError> {
        let mut readings = Vec::new();
        for fan in &self.fans {
            let rpm = fan.rpm_path.as_deref().and_then(read_rpm);
            let duty_percent = fan.pwm_path.as_deref().and_then(read_duty_percent);
            readings.push(FanReading {
                id: fan.id.clone(),
                name: fan.name.clone(),
                rpm,
                duty_percent,
                controllable: fan.controllable,
            });
        }
        Ok(readings)
    }
}

/// Internal view of one `hwmonN` directory.
struct ChipDir {
    path: PathBuf,
    dir_name: String,
    chip: String,
}

/// Fan-related paths collected before IDs and names are assigned.
#[derive(Default)]
struct FanPaths {
    rpm: Option<PathBuf>,
    pwm: Option<PathBuf>,
    pwm_enable: Option<PathBuf>,
}

fn discover_sensors(chip: &ChipDir, prefix: &str) -> Vec<HwmonSensor> {
    let entries = match std::fs::read_dir(&chip.path) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut sensors = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let Some(index) = numbered_name(&file_name, "temp", "_input") else {
            continue;
        };
        let input_path = entry.path();
        let label = read_trimmed(&chip.path.join(format!("temp{index}_label")));
        let name = label.unwrap_or_else(|| format!("{} temp{index}", chip.chip));
        sensors.push(HwmonSensor {
            id: format!("{prefix}:temp{index}"),
            name,
            chip: chip.chip.clone(),
            index,
            input_path,
        });
    }
    sensors
}

fn discover_fans(chip: &ChipDir, prefix: &str) -> Vec<HwmonFan> {
    let entries = match std::fs::read_dir(&chip.path) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut paths: BTreeMap<u32, FanPaths> = BTreeMap::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if let Some(index) = numbered_name(&file_name, "fan", "_input") {
            paths.entry(index).or_default().rpm = Some(entry.path());
        } else if let Some(index) = numbered_name(&file_name, "pwm", "") {
            paths.entry(index).or_default().pwm = Some(entry.path());
        } else if let Some(index) = numbered_name(&file_name, "pwm", "_enable") {
            paths.entry(index).or_default().pwm_enable = Some(entry.path());
        }
    }

    let mut fans = Vec::new();
    for (index, parts) in paths {
        // A stray `pwmN_enable` without `pwmN` (or a tach input) is not a fan.
        if parts.rpm.is_none() && parts.pwm.is_none() {
            continue;
        }
        let name = read_trimmed(&chip.path.join(format!("fan{index}_label")))
            .or_else(|| read_trimmed(&chip.path.join(format!("pwm{index}_label"))))
            .unwrap_or_else(|| format!("{} fan{index}", chip.chip));
        let controllable = parts.pwm.is_some() && parts.pwm_enable.is_some();
        fans.push(HwmonFan {
            id: format!("{prefix}:pwm{index}"),
            name,
            chip: chip.chip.clone(),
            index,
            rpm_path: parts.rpm,
            pwm_path: parts.pwm,
            pwm_enable_path: parts.pwm_enable,
            controllable,
        });
    }
    fans
}

/// Match `<prefix><digits><suffix>` and return the digits as an index.
fn numbered_name(name: &str, prefix: &str, suffix: &str) -> Option<u32> {
    let digits = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn read_temperature(path: &Path) -> Option<f64> {
    let millidegrees = std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()?;
    let temp_c = millidegrees as f64 / 1000.0;
    if (-40.0..=150.0).contains(&temp_c) {
        Some(temp_c)
    } else {
        None
    }
}

fn read_rpm(path: &Path) -> Option<u32> {
    let rpm = std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()?;
    if (0..=20_000).contains(&rpm) {
        u32::try_from(rpm).ok()
    } else {
        None
    }
}

fn read_duty_percent(path: &Path) -> Option<f64> {
    let pwm = std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()?;
    if (0..=255).contains(&pwm) {
        Some(pwm as f64 / 255.0 * 100.0)
    } else {
        None
    }
}

fn read_u8(path: &Path) -> Result<u8, HwmonError> {
    let text = std::fs::read_to_string(path).map_err(|source| HwmonError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    text.trim()
        .parse::<u8>()
        .map_err(|_| HwmonError::NoRestoreState(path.to_string_lossy().into_owned()))
}

fn write_value(path: &Path, value: &str) -> Result<(), HwmonError> {
    std::fs::write(path, value).map_err(|source| HwmonError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Convert a duty percentage to the hwmon 0..=255 range.
///
/// Finite values outside `0..=100` are clamped. NaN and infinities are
/// rejected because silently turning them into 0 or 255 could either stop a
/// fan or unexpectedly run it at full speed.
fn duty_to_pwm(duty_percent: f64) -> Result<u8, HwmonError> {
    if !duty_percent.is_finite() {
        return Err(HwmonError::InvalidDuty(duty_percent));
    }
    let clamped = duty_percent.clamp(0.0, 100.0);
    Ok((clamped / 100.0 * 255.0).round() as u8)
}

/// PWM writer that restores the original hwmon mode on drop.
///
/// # Control model
///
/// [`HwmonController::new`] only reads `pwmN_enable`; it never writes. Manual
/// mode is entered explicitly with [`HwmonController::enable_manual`] or
/// lazily from [`HwmonController::apply`]. Lazy enable keeps the single
/// caller that applies a duty from having to sequence two calls, but PWM is
/// only ever written after the corresponding `pwmN_enable` was successfully
/// set to `1`. If enabling fails, no PWM write is attempted.
///
/// Only fans that were actually switched to manual mode are restored. A
/// controller that was never put into manual mode writes nothing on drop.
#[derive(Debug, Default)]
pub struct HwmonController {
    inventory: HwmonInventory,
    manual: bool,
    /// Original `pwmN_enable` values recorded at construction or first enable.
    originals: BTreeMap<String, u8>,
    /// Fan IDs whose `pwmN_enable` this controller changed to `1`.
    enabled: BTreeSet<String>,
}

impl HwmonController {
    /// Create a controller for an inventory.
    ///
    /// This reads and remembers each fan's current `pwmN_enable` value but
    /// never writes to sysfs.
    pub fn new(inventory: HwmonInventory) -> Self {
        let mut originals = BTreeMap::new();
        for fan in &inventory.fans {
            if let Some(path) = &fan.pwm_enable_path {
                if let Ok(value) = read_u8(path) {
                    originals.insert(fan.id.clone(), value);
                }
            }
        }
        Self {
            inventory,
            manual: false,
            originals,
            enabled: BTreeSet::new(),
        }
    }

    /// The discovered inventory this controller operates on.
    pub fn inventory(&self) -> &HwmonInventory {
        &self.inventory
    }

    /// Whether manual PWM mode is active.
    pub fn manual(&self) -> bool {
        self.manual
    }

    /// Switch every controllable fan to manual PWM mode.
    ///
    /// Original enable values are recorded before any write. The call is
    /// idempotent: once manual mode is active it returns `Ok(())` without
    /// writing again. A fan without both `pwmN` and `pwmN_enable` is ignored
    /// here and rejected by [`Self::apply`].
    pub fn enable_manual(&mut self) -> Result<(), HwmonError> {
        if self.manual {
            return Ok(());
        }

        let fans: Vec<(String, PathBuf)> = self
            .inventory
            .fans
            .iter()
            .filter(|fan| fan.controllable)
            .map(|fan| (fan.id.clone(), fan.pwm_enable_path.clone()))
            .filter_map(|(id, path)| path.map(|path| (id, path)))
            .collect();

        // Record every missing original before writing anything. If a value
        // cannot be recovered we return an error while sysfs is still pristine.
        let mut missing = Vec::new();
        for (id, path) in &fans {
            if !self.originals.contains_key(id) {
                missing.push((id.clone(), read_u8(path)?));
            }
        }
        self.originals.extend(missing);

        for (id, path) in &fans {
            write_value(path, "1\n")?;
            self.enabled.insert(id.clone());
        }
        self.manual = true;
        Ok(())
    }

    /// Switch one fan to manual PWM mode, recording its original mode.
    ///
    /// Unlike [`Self::enable_manual`], this does not touch any other fan. The
    /// node uses it so a policy that controls one fan cannot silently take
    /// unrelated fans out of firmware control.
    pub fn enable_manual_for(&mut self, fan_id: &str) -> Result<(), HwmonError> {
        if self.enabled.contains(fan_id) {
            return Ok(());
        }
        let (enable_path, controllable) = self
            .inventory
            .fans
            .iter()
            .find(|fan| fan.id == fan_id)
            .map(|fan| (fan.pwm_enable_path.clone(), fan.controllable))
            .ok_or_else(|| HwmonError::UnknownFan(fan_id.to_owned()))?;
        if !controllable {
            return Err(HwmonError::NotControllable(fan_id.to_owned()));
        }
        let enable_path =
            enable_path.ok_or_else(|| HwmonError::NotControllable(fan_id.to_owned()))?;
        if !self.originals.contains_key(fan_id) {
            let original = read_u8(&enable_path)?;
            self.originals.insert(fan_id.to_owned(), original);
        }
        write_value(&enable_path, "1\n")?;
        self.enabled.insert(fan_id.to_owned());
        self.manual = true;
        Ok(())
    }

    /// Apply a duty percentage to one fan.
    ///
    /// The duty is clamped to `0..=100` and converted to `0..=255`. NaN and
    /// infinities return [`HwmonError::InvalidDuty`]. If this fan is not in
    /// manual mode yet, only this fan is switched to manual (its original mode
    /// is recorded first), then `pwmN` is written. A fan lacking `pwmN` or
    /// `pwmN_enable` returns [`HwmonError::NotControllable`] without writing.
    pub fn apply(&mut self, fan_id: &str, duty_percent: f64) -> Result<(), HwmonError> {
        let pwm = duty_to_pwm(duty_percent)?;

        let fan = self
            .inventory
            .fans
            .iter()
            .find(|fan| fan.id == fan_id)
            .ok_or_else(|| HwmonError::UnknownFan(fan_id.to_owned()))?;
        if !fan.controllable {
            return Err(HwmonError::NotControllable(fan_id.to_owned()));
        }
        let pwm_path = fan
            .pwm_path
            .clone()
            .ok_or_else(|| HwmonError::NotControllable(fan_id.to_owned()))?;
        if fan.pwm_enable_path.is_none() {
            return Err(HwmonError::NotControllable(fan_id.to_owned()));
        }

        // Never touch PWM unless this fan's enable write (and original
        // recording) succeeded.
        self.enable_manual_for(fan_id)?;
        write_value(&pwm_path, &format!("{pwm}\n"))
    }

    /// Restore every fan changed by this controller to its original mode.
    ///
    /// Fans are restored in ID order. Successfully restored fans are forgotten
    /// immediately, so the call is idempotent and a partial failure can be
    /// retried (including from `Drop`). Once nothing remains to restore,
    /// manual state and recorded originals are cleared.
    pub fn restore(&mut self) -> Result<(), HwmonError> {
        let mut first_error = None;
        let mut restored = Vec::new();

        for id in &self.enabled {
            let Some(original) = self.originals.get(id).copied() else {
                first_error.get_or_insert_with(|| HwmonError::NoRestoreState(id.clone()));
                continue;
            };
            let path = self
                .inventory
                .fans
                .iter()
                .find(|fan| &fan.id == id)
                .and_then(|fan| fan.pwm_enable_path.clone());
            let Some(path) = path else {
                first_error.get_or_insert_with(|| HwmonError::NotControllable(id.clone()));
                continue;
            };
            match write_value(&path, &format!("{original}\n")) {
                Ok(()) => restored.push(id.clone()),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }

        for id in restored {
            self.enabled.remove(&id);
        }
        if self.enabled.is_empty() {
            self.manual = false;
            self.originals.clear();
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for HwmonController {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    /// Self-cleaning temporary hwmon root.
    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new(tag: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock before epoch")
                .as_nanos();
            let unique = format!(
                "ozma-hwmon-{}-{tag}-{nanos}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&path).expect("create temporary hwmon root");
            Self { path }
        }

        fn chip(&self, dir: &str, name: Option<&str>) -> PathBuf {
            let path = self.path.join(dir);
            std::fs::create_dir_all(&path).expect("create hwmon chip directory");
            if let Some(name) = name {
                Self::write(&path.join("name"), name);
            }
            path
        }

        fn write(path: &Path, value: impl std::fmt::Display) {
            std::fs::write(path, format!("{value}\n")).expect("write fixture");
        }

        fn read(path: &Path) -> String {
            std::fs::read_to_string(path)
                .expect("read fixture")
                .trim()
                .to_owned()
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn inventory(root: &Path) -> HwmonInventory {
        HwmonInventory::discover(root).expect("discover fixture")
    }

    fn pwm_percent(pwm: u8) -> f64 {
        f64::from(pwm) / 255.0 * 100.0
    }

    #[test]
    fn discovers_sensors_and_fans_across_chips() {
        let root = TempRoot::new("discover");

        let core = root.chip("hwmon0", Some("coretemp"));
        TempRoot::write(&core.join("temp1_input"), 45_000);
        TempRoot::write(&core.join("temp1_label"), "Package id 0");
        TempRoot::write(&core.join("temp2_input"), 38_000);
        TempRoot::write(&core.join("fan1_input"), 1_200);
        TempRoot::write(&core.join("fan1_label"), "CPU Fan");
        TempRoot::write(&core.join("pwm1"), 128);
        TempRoot::write(&core.join("pwm1_enable"), 2);

        let board = root.chip("hwmon1", Some("nct6798"));
        TempRoot::write(&board.join("temp1_input"), 30_000);
        TempRoot::write(&board.join("fan1_input"), 800);
        TempRoot::write(&board.join("pwm2"), 200);
        TempRoot::write(&board.join("pwm2_enable"), 1);
        TempRoot::write(&board.join("pwm2_label"), "Chassis");

        let inv = inventory(&root.path);
        assert_eq!(inv.sensors.len(), 3);
        assert_eq!(inv.fans.len(), 3);

        let package = inv
            .sensors
            .iter()
            .find(|sensor| sensor.id == "hwmon:coretemp:temp1")
            .expect("package sensor");
        assert_eq!(package.name, "Package id 0");
        assert_eq!(package.chip, "coretemp");
        assert_eq!(package.index, 1);

        let unlabelled = inv
            .sensors
            .iter()
            .find(|sensor| sensor.id == "hwmon:coretemp:temp2")
            .expect("unlabelled sensor");
        assert_eq!(unlabelled.name, "coretemp temp2");

        let cpu_fan = inv
            .fans
            .iter()
            .find(|fan| fan.id == "hwmon:coretemp:pwm1")
            .expect("cpu fan");
        assert_eq!(cpu_fan.name, "CPU Fan");
        assert!(cpu_fan.controllable);
        assert_eq!(
            cpu_fan.rpm_path.as_deref(),
            Some(core.join("fan1_input").as_path())
        );
        assert_eq!(
            cpu_fan.pwm_path.as_deref(),
            Some(core.join("pwm1").as_path())
        );
        assert_eq!(
            cpu_fan.pwm_enable_path.as_deref(),
            Some(core.join("pwm1_enable").as_path())
        );

        let tach_only = inv
            .fans
            .iter()
            .find(|fan| fan.id == "hwmon:nct6798:pwm1")
            .expect("tach-only fan");
        assert_eq!(tach_only.name, "nct6798 fan1");
        assert!(!tach_only.controllable);
        assert!(tach_only.pwm_path.is_none());
        assert!(tach_only.pwm_enable_path.is_none());

        let pwm_only = inv
            .fans
            .iter()
            .find(|fan| fan.id == "hwmon:nct6798:pwm2")
            .expect("pwm-only fan");
        assert_eq!(pwm_only.name, "Chassis");
        assert!(pwm_only.controllable);
        assert!(pwm_only.rpm_path.is_none());

        let sensors = inv.read_sensors().expect("read sensors");
        assert_eq!(sensors.len(), 3);
        assert!(sensors.iter().all(|reading| reading.age_secs == 0.0));
        let temps: BTreeMap<&str, f64> = sensors
            .iter()
            .map(|reading| (reading.id.as_str(), reading.temp_c))
            .collect();
        assert_eq!(temps["hwmon:coretemp:temp1"], 45.0);
        assert_eq!(temps["hwmon:coretemp:temp2"], 38.0);
        assert_eq!(temps["hwmon:nct6798:temp1"], 30.0);

        let readings = inv.read_fans().expect("read fans");
        assert_eq!(readings.len(), 3);
        let cpu = readings
            .iter()
            .find(|reading| reading.id == "hwmon:coretemp:pwm1")
            .expect("cpu reading");
        assert_eq!(cpu.rpm, Some(1_200));
        assert!((cpu.duty_percent.expect("cpu duty") - pwm_percent(128)).abs() < 1e-9);
        assert!(cpu.controllable);

        let tach = readings
            .iter()
            .find(|reading| reading.id == "hwmon:nct6798:pwm1")
            .expect("tach reading");
        assert_eq!(tach.rpm, Some(800));
        assert_eq!(tach.duty_percent, None);
        assert!(!tach.controllable);

        let pwm = readings
            .iter()
            .find(|reading| reading.id == "hwmon:nct6798:pwm2")
            .expect("pwm reading");
        assert_eq!(pwm.rpm, None);
        assert!((pwm.duty_percent.expect("pwm duty") - pwm_percent(200)).abs() < 1e-9);
        assert!(pwm.controllable);
    }

    #[test]
    fn duplicate_chip_names_are_disambiguated_with_directory() {
        let root = TempRoot::new("duplicate");
        for (dir, temp) in [("hwmon2", 50_000), ("hwmon3", 60_000)] {
            let chip = root.chip(dir, Some("amdgpu"));
            TempRoot::write(&chip.join("temp1_input"), temp);
            TempRoot::write(&chip.join("pwm1"), 100);
            TempRoot::write(&chip.join("pwm1_enable"), 2);
        }

        let inv = inventory(&root.path);
        let ids: BTreeSet<&str> = inv
            .sensors
            .iter()
            .map(|sensor| sensor.id.as_str())
            .chain(inv.fans.iter().map(|fan| fan.id.as_str()))
            .collect();

        assert!(ids.contains("hwmon:amdgpu@hwmon2:temp1"));
        assert!(ids.contains("hwmon:amdgpu@hwmon2:pwm1"));
        assert!(ids.contains("hwmon:amdgpu@hwmon3:temp1"));
        assert!(ids.contains("hwmon:amdgpu@hwmon3:pwm1"));
        assert_eq!(ids.len(), 4, "IDs must stay unique");
        assert!(inv.sensors.iter().all(|sensor| sensor.chip == "amdgpu"));
    }

    #[test]
    fn invalid_and_out_of_range_readings_are_skipped() {
        let root = TempRoot::new("invalid");
        let chip = root.chip("hwmon0", Some("testchip"));
        TempRoot::write(&chip.join("temp1_input"), 200_000);
        TempRoot::write(&chip.join("temp2_input"), -50_000);
        TempRoot::write(&chip.join("temp3_input"), "not-a-number");
        TempRoot::write(&chip.join("temp4_input"), 45_000);
        TempRoot::write(&chip.join("temp5_input"), 150_000);

        TempRoot::write(&chip.join("fan1_input"), 25_000);
        TempRoot::write(&chip.join("fan2_input"), 1_000);
        TempRoot::write(&chip.join("pwm2"), 300);
        TempRoot::write(&chip.join("fan3_input"), 900);
        TempRoot::write(&chip.join("pwm3"), 0);

        let inv = inventory(&root.path);
        let sensors = inv.read_sensors().expect("read sensors");
        let temps: BTreeMap<&str, f64> = sensors
            .iter()
            .map(|reading| (reading.id.as_str(), reading.temp_c))
            .collect();
        assert_eq!(sensors.len(), 2);
        assert_eq!(temps["hwmon:testchip:temp4"], 45.0);
        assert_eq!(temps["hwmon:testchip:temp5"], 150.0);

        let fans = inv.read_fans().expect("read fans");
        let fan1 = fans
            .iter()
            .find(|reading| reading.id == "hwmon:testchip:pwm1")
            .expect("fan1");
        assert_eq!(fan1.rpm, None, "out-of-range RPM is omitted");

        let fan2 = fans
            .iter()
            .find(|reading| reading.id == "hwmon:testchip:pwm2")
            .expect("fan2");
        assert_eq!(fan2.rpm, Some(1_000));
        assert_eq!(fan2.duty_percent, None, "out-of-range PWM is omitted");

        let fan3 = fans
            .iter()
            .find(|reading| reading.id == "hwmon:testchip:pwm3")
            .expect("fan3");
        assert_eq!(fan3.duty_percent, Some(0.0));
    }

    #[test]
    fn controller_records_enable_and_writes_pwm() {
        let root = TempRoot::new("controller");
        let chip = root.chip("hwmon0", Some("nct6798"));
        let pwm = chip.join("pwm1");
        let enable = chip.join("pwm1_enable");
        TempRoot::write(&pwm, 0);
        TempRoot::write(&enable, 2);
        TempRoot::write(&chip.join("fan1_input"), 1_000);

        let inv = inventory(&root.path);
        let fan_id = inv.fans[0].id.clone();
        let mut controller = HwmonController::new(inv);

        assert!(!controller.manual());
        assert_eq!(TempRoot::read(&enable), "2", "new must not write");

        controller.enable_manual().expect("enable manual");
        assert!(controller.manual());
        assert_eq!(TempRoot::read(&enable), "1");

        controller.enable_manual().expect("idempotent enable");
        assert_eq!(TempRoot::read(&enable), "1");

        controller.apply(&fan_id, 100.0).expect("apply 100");
        assert_eq!(TempRoot::read(&pwm), "255");
        controller.apply(&fan_id, 0.0).expect("apply 0");
        assert_eq!(TempRoot::read(&pwm), "0");
        controller.apply(&fan_id, 50.0).expect("apply 50");
        assert_eq!(TempRoot::read(&pwm), "128");

        controller.restore().expect("restore");
        assert!(!controller.manual());
        assert_eq!(TempRoot::read(&enable), "2");

        controller.restore().expect("idempotent restore");
        assert_eq!(TempRoot::read(&enable), "2");
    }

    #[test]
    fn non_controllable_fans_are_rejected() {
        let root = TempRoot::new("readonly");
        let tach = root.chip("hwmon0", Some("tachchip"));
        TempRoot::write(&tach.join("fan1_input"), 900);

        let pwm = root.chip("hwmon1", Some("pwmchip"));
        let pwm_file = pwm.join("pwm1");
        TempRoot::write(&pwm_file, 77);
        TempRoot::write(&pwm.join("fan1_input"), 900);

        let inv = inventory(&root.path);
        let mut controller = HwmonController::new(inv);

        let ids: Vec<String> = controller
            .inventory()
            .fans
            .iter()
            .map(|fan| {
                assert!(!fan.controllable);
                fan.id.clone()
            })
            .collect();
        for id in ids {
            match controller.apply(&id, 50.0) {
                Err(HwmonError::NotControllable(rejected)) => assert_eq!(rejected, id),
                other => panic!("expected NotControllable, got {other:?}"),
            }
        }
        assert!(!controller.manual());
        assert_eq!(TempRoot::read(&pwm_file), "77", "PWM must be untouched");
    }

    #[test]
    fn duty_is_clamped_and_non_finite_is_rejected() {
        let root = TempRoot::new("duty");
        let chip = root.chip("hwmon0", Some("dutchip"));
        let pwm = chip.join("pwm1");
        let enable = chip.join("pwm1_enable");
        TempRoot::write(&pwm, 0);
        TempRoot::write(&enable, 2);

        let inv = inventory(&root.path);
        let fan_id = inv.fans[0].id.clone();
        let mut controller = HwmonController::new(inv);
        controller.enable_manual().expect("enable manual");

        for duty in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            match controller.apply(&fan_id, duty) {
                Err(HwmonError::InvalidDuty(rejected)) => {
                    assert!((rejected.is_nan() && duty.is_nan()) || rejected == duty);
                }
                other => panic!("expected InvalidDuty for {duty}, got {other:?}"),
            }
        }
        assert_eq!(TempRoot::read(&pwm), "0", "invalid duty must not write");

        controller.apply(&fan_id, -25.0).expect("clamped low");
        assert_eq!(TempRoot::read(&pwm), "0");
        controller.apply(&fan_id, 150.0).expect("clamped high");
        assert_eq!(TempRoot::read(&pwm), "255");
        controller.apply(&fan_id, 50.0).expect("half");
        assert_eq!(TempRoot::read(&pwm), "128");
    }

    #[test]
    fn drop_restores_original_enable() {
        let root = TempRoot::new("drop");
        let chip = root.chip("hwmon0", Some("dropchip"));
        let enable = chip.join("pwm1_enable");
        TempRoot::write(&chip.join("pwm1"), 0);
        TempRoot::write(&enable, 3);

        {
            let inv = inventory(&root.path);
            let fan_id = inv.fans[0].id.clone();
            let mut controller = HwmonController::new(inv);
            controller.enable_manual().expect("enable manual");
            controller.apply(&fan_id, 75.0).expect("apply");
            assert_eq!(TempRoot::read(&enable), "1");
        }

        assert_eq!(TempRoot::read(&enable), "3", "Drop must restore");
    }

    #[test]
    fn missing_name_uses_directory_and_missing_root_is_empty() {
        let root = TempRoot::new("missing-name");
        let chip = root.chip("hwmon7", None);
        TempRoot::write(&chip.join("temp1_input"), 42_000);

        let inv = inventory(&root.path);
        assert_eq!(inv.sensors.len(), 1);
        assert_eq!(inv.sensors[0].chip, "hwmon7");
        assert_eq!(inv.sensors[0].id, "hwmon:hwmon7:temp1");
        assert_eq!(inv.sensors[0].name, "hwmon7 temp1");

        let absent = HwmonInventory::discover(&root.path.join("does-not-exist"))
            .expect("missing root is empty");
        assert!(absent.sensors.is_empty());
        assert!(absent.fans.is_empty());
    }
}
