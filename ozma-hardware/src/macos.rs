// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! macOS hardware and sensor collection (system_profiler + sysctl + powermetrics).
//!
//! The collector is deliberately command-line based: everything here works with
//! the tools shipped by macOS and does not require a native SDK or extra crates.
//! Sensor data (fans and die temperatures) comes from `powermetrics`, which is
//! only usable as root. When it is unavailable the sensor lists stay empty
//! rather than being filled with synthetic values.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::Value;

use crate::{
    BatteryInfo, CpuCoreInfo, CpuPackageInfo, FanReading, GpuInfo, HardwareCollector,
    HardwareSnapshot, MemorySlotInfo, MemoryTiming, MotherboardInfo, NetworkInfo, StorageInfo,
};

/// macOS collector.
pub struct MacOsHardwareCollector;

impl MacOsHardwareCollector {
    /// Create a macOS collector.
    pub fn new() -> Self {
        Self
    }
}

impl Default for MacOsHardwareCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl HardwareCollector for MacOsHardwareCollector {
    fn collect(&mut self) -> Result<HardwareSnapshot> {
        let memsize = sysctl("hw.memsize").and_then(|value| value.parse::<u64>().ok());

        let mut snapshot = HardwareSnapshot {
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs_f64())
                .unwrap_or(0.0),
            hostname: collect_hostname(),
            os: "macOS".to_string(),
            os_version: collect_os_version(),
            ..HardwareSnapshot::default()
        };

        snapshot.cpu = collect_cpu();
        snapshot.memory_slots = collect_memory(memsize);
        snapshot.total_memory_gb = snapshot.memory_slots.iter().map(|slot| slot.size_gb).sum();

        snapshot.gpus = collect_gpus();
        snapshot.storage = collect_storage();
        snapshot.batteries = collect_battery();
        snapshot.motherboard = collect_motherboard();
        snapshot.network = collect_network();

        let (fans, cpu_temp, gpu_temp) = collect_sensors();
        snapshot.fans = fans;
        for fan in &snapshot.fans {
            snapshot.sensors.push(crate::HardwareSensor {
                id: format!("powermetrics:fan:{}", fan.name),
                chip: "powermetrics".to_string(),
                label: fan.name.clone(),
                kind: "fan".to_string(),
                value: fan.rpm as f32,
                unit: "RPM".to_string(),
                min: None,
                max: None,
            });
        }
        if let Some(temp) = cpu_temp.filter(|temp| *temp > 0.0) {
            snapshot.cpu.package_temp_c = temp;
            snapshot.sensors.push(crate::HardwareSensor {
                id: "powermetrics:cpu:temperature".to_string(),
                chip: "powermetrics".to_string(),
                label: "CPU die temperature".to_string(),
                kind: "temperature".to_string(),
                value: temp,
                unit: "C".to_string(),
                min: None,
                max: None,
            });
        }
        if let Some(temp) = gpu_temp.filter(|temp| *temp > 0.0) {
            if let Some(gpu) = snapshot.gpus.first_mut() {
                gpu.temperature_c = temp;
            }
            snapshot.sensors.push(crate::HardwareSensor {
                id: "powermetrics:gpu:temperature".to_string(),
                chip: "powermetrics".to_string(),
                label: "GPU temperature".to_string(),
                kind: "temperature".to_string(),
                value: temp,
                unit: "C".to_string(),
                min: None,
                max: None,
            });
        }

        Ok(snapshot)
    }
}

/// Parse `sysctl` output.
///
/// Keyed output (`hw.ncpu: 8`) is split on the first colon. Lines without a
/// colon are bare values as printed by `sysctl -n <key>` and are stored under
/// the empty key so callers can still recover the value.
pub fn parse_sysctl(text: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some((key, value)) = line.split_once(':') {
            values.insert(key.trim().to_string(), value.trim().to_string());
        } else {
            values.insert(String::new(), line.to_string());
        }
    }

    values
}

/// Parse a `system_profiler -json` payload.
///
/// Malformed or empty input yields [`Value::Null`] so callers can treat it the
/// same as a failed command.
pub fn parse_system_profiler_json(payload: &str) -> Value {
    serde_json::from_str(payload).unwrap_or(Value::Null)
}

/// Parse the memory section of `system_profiler -json SPMemoryDataType`.
///
/// Both the flat and the nested (`_items`) layouts are accepted, as are payloads
/// that are already just the `SPMemoryDataType` array.
pub fn parse_memory_profile(value: &Value) -> Vec<MemorySlotInfo> {
    let entries = leaf_entries(section_entries(value, "SPMemoryDataType"));
    let mut slots = Vec::new();

    for entry in entries {
        let has_spd = entry.get("dimm_size").is_some()
            || entry.get("dimm_type").is_some()
            || entry.get("dimm_speed").is_some()
            || entry.get("dimm_manufacturer").is_some()
            || entry.get("dimm_part_number").is_some()
            || entry.get("dimm_serial_number").is_some();

        if !has_spd {
            continue;
        }

        let size_gb = first_string_field(entry, &["dimm_size"])
            .and_then(|value| parse_capacity_gb(&value))
            .unwrap_or(0.0);
        if size_gb <= 0.0 {
            continue;
        }
        let speed_mhz = number_field(entry, &["dimm_speed", "dimm_speed_mhz"])
            .map(|value| value.max(0.0) as u32)
            .unwrap_or(0);
        let configured_speed_mhz = number_field(
            entry,
            &[
                "dimm_configured_speed",
                "configured_speed",
                "dimm_configured_speed_mhz",
            ],
        )
        .map(|value| value.max(0.0) as u32)
        .unwrap_or(speed_mhz);
        let voltage_v = number_field(entry, &["dimm_voltage", "configured_voltage"])
            .map(|value| value.max(0.0) as f32)
            .unwrap_or(0.0);

        slots.push(MemorySlotInfo {
            slot: first_string_field(entry, &["dimm_slot", "_name"]).unwrap_or_default(),
            size_gb,
            memory_type: first_string_field(entry, &["dimm_type", "memory_type"])
                .unwrap_or_default(),
            speed_mhz,
            configured_speed_mhz,
            manufacturer: first_string_field(entry, &["dimm_manufacturer", "manufacturer"])
                .unwrap_or_default(),
            part_number: first_string_field(entry, &["dimm_part_number", "part_number"])
                .unwrap_or_default(),
            serial_number: first_string_field(entry, &["dimm_serial_number", "serial_number"])
                .unwrap_or_default(),
            form_factor: first_string_field(entry, &["dimm_form_factor", "form_factor"])
                .unwrap_or_default(),
            rank: first_string_field(entry, &["dimm_rank", "rank"]).unwrap_or_default(),
            bank: first_string_field(entry, &["dimm_bank", "bank"]).unwrap_or_default(),
            channel: first_string_field(entry, &["dimm_channel", "channel"]).unwrap_or_default(),
            timing: MemoryTiming {
                voltage_v,
                ..MemoryTiming::default()
            },
            ..MemorySlotInfo::default()
        });
    }

    slots
}

/// Parse the display/GPU section of `system_profiler -json SPDisplaysDataType`.
pub fn parse_displays_profile(value: &Value) -> Vec<GpuInfo> {
    let entries = section_entries(value, "SPDisplaysDataType");
    let mut gpus = Vec::new();

    for entry in leaf_entries(entries) {
        let model = first_string_field(entry, &["sppci_model", "_name", "spdisplays_model"])
            .unwrap_or_default();
        let vendor_raw =
            first_string_field(entry, &["sppci_vendor", "spdisplays_vendor"]).unwrap_or_default();
        let vram_field = first_string_field(
            entry,
            &["spdisplays_vram", "sppci_vram", "spdisplays_vram_shared"],
        );

        if model.is_empty() && vendor_raw.is_empty() && vram_field.is_none() {
            continue;
        }

        let vram_gb = vram_field
            .and_then(|value| {
                if value.eq_ignore_ascii_case("shared") {
                    Some(0.0)
                } else {
                    parse_capacity_gb(&value)
                }
            })
            .unwrap_or(0.0);
        let metal = first_string_field(entry, &["spdisplays_metal", "spdisplays_metal_support"])
            .unwrap_or_default();

        gpus.push(GpuInfo {
            index: gpus.len() as u32,
            vendor: detect_gpu_vendor(&vendor_raw, &model),
            model,
            vram_gb,
            driver_version: metal,
            ..GpuInfo::default()
        });
    }

    gpus
}

/// Parse the storage section of `system_profiler -json SPStorageDataType`.
pub fn parse_storage_profile(value: &Value) -> Vec<StorageInfo> {
    let entries = leaf_entries(section_entries(value, "SPStorageDataType"));
    let mut storage = Vec::new();

    for entry in entries {
        let device =
            first_string_field(entry, &["bsd_name", "device_name", "_name"]).unwrap_or_default();
        let model =
            first_string_field(entry, &["device_model", "_name", "model"]).unwrap_or_default();
        let serial = first_string_field(entry, &["device_serial", "serial_number", "serial"])
            .unwrap_or_default();
        let firmware =
            first_string_field(entry, &["device_revision", "firmware_revision", "firmware"])
                .unwrap_or_default();
        let capacity_bytes = first_bytes_field(
            entry,
            &[
                "com.apple.diskmanagement.media.total-size",
                "size_in_bytes",
                "capacity_bytes",
                "media_total_size",
            ],
        )
        .unwrap_or(0);
        let medium = first_string_field(
            entry,
            &[
                "com.apple.diskmanagement.media.medium-type",
                "spstorage_medium_type",
                "medium_type",
                "spstorage_type",
            ],
        )
        .unwrap_or_default();

        let has_content =
            !device.is_empty() || !model.is_empty() || !serial.is_empty() || capacity_bytes > 0;
        if !has_content {
            continue;
        }

        if storage.iter().any(|existing: &StorageInfo| {
            existing.device == device
                && existing.model == model
                && existing.capacity_bytes == capacity_bytes
        }) {
            continue;
        }

        storage.push(StorageInfo {
            device,
            model,
            serial,
            firmware,
            interface: storage_interface(entry, &medium),
            form_factor: first_string_field(entry, &["spstorage_form_factor", "form_factor"])
                .unwrap_or_default(),
            capacity_bytes,
            rotational: is_rotational(entry, &medium),
            rpm: number_field(entry, &["spstorage_rotation_rate", "rotation_rate", "rpm"])
                .map(|value| value.max(0.0) as u32)
                .unwrap_or(0),
            ..StorageInfo::default()
        });
    }

    storage
}

/// Parse the battery section of `system_profiler -json SPPowerDataType`.
pub fn parse_power_profile(value: &Value) -> Vec<BatteryInfo> {
    let entries = section_entries(value, "SPPowerDataType");
    let sections = [
        "sppower_battery_health_info",
        "sppower_battery_charge_info",
        "sppower_battery_model_info",
    ];
    let mut batteries = Vec::new();

    for entry in entries {
        let info = entry
            .get("sppower_battery_information")
            .and_then(first_object)
            .filter(|value| battery_info_has_data(value))
            .unwrap_or(entry);
        if !battery_info_has_data(info) {
            continue;
        }

        let voltage_mv = lookup_f64(info, &sections, &["sppower_battery_voltage", "voltage"])
            .filter(|value| *value > 0.0);
        let amperage_ma = lookup_f64(info, &sections, &["sppower_battery_amperage", "amperage"]);
        let state_of_charge = lookup_f64(
            info,
            &sections,
            &[
                "sppower_battery_state_of_charge",
                "state_of_charge",
                "sppower_battery_charge_percent",
            ],
        );

        let design_mwh = lookup_f64(
            info,
            &sections,
            &[
                "sppower_battery_design_capacity_mwh",
                "sppower_design_capacity_mwh",
            ],
        )
        .map(clamp_u32)
        .or_else(|| {
            lookup_f64(
                info,
                &sections,
                &[
                    "sppower_battery_design_capacity",
                    "sppower_design_capacity",
                    "design_capacity",
                ],
            )
            .map(|value| capacity_mah_to_mwh(value, voltage_mv))
        })
        .unwrap_or(0);

        let mut full_mwh = lookup_f64(
            info,
            &sections,
            &[
                "sppower_battery_full_charge_capacity_mwh",
                "sppower_battery_max_capacity_mwh",
            ],
        )
        .map(clamp_u32)
        .or_else(|| {
            lookup_f64(
                info,
                &sections,
                &[
                    "sppower_battery_full_charge_capacity",
                    "sppower_battery_max_capacity",
                    "sppower_battery_charge_capacity",
                    "sppower_maximum_capacity",
                    "full_charge_capacity",
                    "max_capacity",
                ],
            )
            .map(|value| capacity_mah_to_mwh(value, voltage_mv))
        })
        .unwrap_or(0);

        if full_mwh == 0 && design_mwh > 0 {
            if let Some(percent) = lookup_f64(
                info,
                &sections,
                &[
                    "sppower_battery_health_maximum_capacity",
                    "sppower_battery_maximum_capacity",
                    "maximum_capacity",
                ],
            )
            .filter(|value| *value > 0.0)
            {
                full_mwh = clamp_u32(f64::from(design_mwh) * percent / 100.0);
            }
        }

        let mut now_mwh = lookup_f64(
            info,
            &sections,
            &[
                "sppower_battery_current_capacity_mwh",
                "sppower_battery_charge_capacity_mwh",
            ],
        )
        .map(clamp_u32)
        .or_else(|| {
            lookup_f64(
                info,
                &sections,
                &[
                    "sppower_battery_current_capacity",
                    "sppower_current_capacity",
                    "sppower_battery_charge_remaining",
                    "current_capacity",
                    "charge_remaining",
                ],
            )
            .map(|value| capacity_mah_to_mwh(value, voltage_mv))
        })
        .unwrap_or(0);

        if now_mwh == 0 && full_mwh > 0 {
            if let Some(percent) = state_of_charge.filter(|value| *value >= 0.0) {
                now_mwh = clamp_u32(f64::from(full_mwh) * percent / 100.0);
            }
        }
        if full_mwh == 0 && now_mwh > 0 {
            if let Some(percent) = state_of_charge.filter(|value| *value > 0.0) {
                full_mwh = clamp_u32(f64::from(now_mwh) * 100.0 / percent);
            }
        }

        let charge_rate_mw = lookup_f64(
            info,
            &sections,
            &[
                "sppower_battery_charge_rate_mw",
                "sppower_battery_charge_rate",
            ],
        )
        .map(|value| clamp_u32(value.abs()))
        .or_else(|| {
            amperage_ma
                .zip(voltage_mv)
                .map(|(amps, volts)| clamp_u32((amps * volts / 1000.0).abs()))
        })
        .unwrap_or(0);

        let condition = lookup_string(
            info,
            &sections,
            &[
                "sppower_battery_health",
                "sppower_battery_health_condition",
                "sppower_battery_condition",
                "condition",
            ],
        )
        .unwrap_or_default();
        let charging = lookup_bool(
            info,
            &sections,
            &["sppower_battery_is_charging", "is_charging"],
        )
        .unwrap_or(false);
        let fully_charged = lookup_bool(
            info,
            &sections,
            &["sppower_battery_fully_charged", "fully_charged"],
        )
        .unwrap_or(false);

        let status = if !condition.is_empty() {
            condition
        } else if charging {
            "Charging".to_string()
        } else if fully_charged {
            "Full".to_string()
        } else {
            String::new()
        };

        let name = lookup_string(
            info,
            &sections,
            &["sppower_battery_device_name", "device_name"],
        )
        .or_else(|| {
            entry
                .get("_name")
                .and_then(value_to_string)
                .filter(|name| !name.starts_with("spbattery"))
        })
        .unwrap_or_else(|| "Battery".to_string());

        let technology = lookup_string(
            info,
            &sections,
            &[
                "sppower_battery_technology",
                "sppower_battery_type",
                "sppower_battery_chemistry",
            ],
        )
        .unwrap_or_default();

        let temperature_c = lookup_f64(
            info,
            &sections,
            &["sppower_battery_temperature", "temperature"],
        )
        .and_then(battery_temperature_celsius)
        .unwrap_or(0.0);

        batteries.push(BatteryInfo {
            name,
            status,
            capacity_design_mwh: design_mwh,
            capacity_full_mwh: full_mwh,
            capacity_now_mwh: now_mwh,
            charge_rate_mw,
            voltage_mv: voltage_mv.map(clamp_u32).unwrap_or(0),
            temperature_c,
            cycle_count: lookup_f64(
                info,
                &sections,
                &["sppower_battery_cycle_count", "cycle_count"],
            )
            .map(|value| value.max(0.0) as u32)
            .unwrap_or(0),
            technology,
        });
    }

    batteries
}

/// Parse `powermetrics --samplers smc` output.
///
/// Returns `(fans, cpu_temperature, gpu_temperature)`. Missing values are
/// represented by `None`; the helper never invents a reading.
pub fn parse_powermetrics(text: &str) -> (Vec<FanReading>, Option<f32>, Option<f32>) {
    let mut fans = Vec::new();
    let mut cpu_temp = None;
    let mut gpu_temp = None;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(fan) = parse_fan_line(line) {
            fans.push(fan);
            continue;
        }

        let lower = line.to_ascii_lowercase();
        if !lower.contains("temperature") {
            continue;
        }
        let Some(temperature) = parse_temperature_value(line) else {
            continue;
        };

        if lower.contains("gpu") {
            gpu_temp.get_or_insert(temperature);
        } else if (lower.contains("cpu") || lower.contains("package")) && cpu_temp.is_none() {
            cpu_temp = Some(temperature);
        }
    }

    (fans, cpu_temp, gpu_temp)
}

fn run_command(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn sysctl(key: &str) -> Option<String> {
    let output = run_command("sysctl", &["-n", key])?;
    let value = output.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn sysctl_u32(key: &str) -> Option<u32> {
    sysctl(key)?.parse().ok()
}

fn sysctl_u64(key: &str) -> Option<u64> {
    sysctl(key)?.parse().ok()
}

fn collect_hostname() -> String {
    run_command("hostname", &[])
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_default()
}

fn collect_os_version() -> String {
    run_command("sw_vers", &["-productVersion"])
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| sysctl("kern.osproductversion"))
        .unwrap_or_default()
}

fn collect_cpu() -> CpuPackageInfo {
    let brand = sysctl("machdep.cpu.brand_string")
        .or_else(|| sysctl("hw.model"))
        .unwrap_or_default();
    let physical_cores = sysctl_u32("hw.physicalcpu").unwrap_or(0);
    let logical_cores = sysctl_u32("hw.logicalcpu")
        .or_else(|| sysctl_u32("hw.ncpu"))
        .unwrap_or(physical_cores);
    let frequency_hz = sysctl_u64("hw.cpufrequency").or_else(|| sysctl_u64("hw.cpufrequency_max"));
    let max_frequency_hz = sysctl_u64("hw.cpufrequency_max").or(frequency_hz);
    let clock_mhz = frequency_hz
        .map(|frequency| frequency as f32 / 1_000_000.0)
        .unwrap_or(0.0);
    let boost_clock_mhz = max_frequency_hz
        .map(|frequency| frequency as f32 / 1_000_000.0)
        .unwrap_or(0.0);
    let microcode = sysctl("machdep.cpu.microcode_version").unwrap_or_default();
    let instructions = cpu_instructions();

    let cores = (0..logical_cores)
        .map(|index| CpuCoreInfo {
            index,
            clock_mhz,
            ..CpuCoreInfo::default()
        })
        .collect();

    CpuPackageInfo {
        brand: brand.clone(),
        model: brand,
        socket: String::new(),
        sockets: 1,
        physical_cores,
        logical_cores,
        base_clock_mhz: clock_mhz,
        boost_clock_mhz,
        microcode,
        instructions,
        cores,
        ..CpuPackageInfo::default()
    }
}

fn cpu_instructions() -> Vec<String> {
    let mut instructions = Vec::new();

    for key in [
        "machdep.cpu.features",
        "machdep.cpu.leaf7_features",
        "machdep.cpu.extfeatures",
    ] {
        let Some(flags) = sysctl(key) else {
            continue;
        };
        for flag in flags.split_whitespace() {
            let flag = flag.to_ascii_uppercase();
            if !flag.is_empty() && !instructions.contains(&flag) {
                instructions.push(flag);
            }
        }
    }

    instructions
}

fn collect_memory(memsize: Option<u64>) -> Vec<MemorySlotInfo> {
    let mut slots = run_command("system_profiler", &["-json", "SPMemoryDataType"])
        .map(|text| parse_memory_profile(&parse_system_profiler_json(&text)))
        .unwrap_or_default();

    let has_size = slots.iter().any(|slot| slot.size_gb > 0.0);
    if !has_size {
        if let Some(bytes) = memsize.filter(|bytes| *bytes > 0) {
            return vec![MemorySlotInfo {
                slot: "System memory".to_string(),
                size_gb: bytes as f32 / GIB,
                ..MemorySlotInfo::default()
            }];
        }
        return Vec::new();
    }

    slots.retain(|slot| slot.size_gb > 0.0);
    slots
}

fn collect_gpus() -> Vec<GpuInfo> {
    run_command("system_profiler", &["-json", "SPDisplaysDataType"])
        .map(|text| parse_displays_profile(&parse_system_profiler_json(&text)))
        .unwrap_or_default()
}

fn collect_storage() -> Vec<StorageInfo> {
    run_command("system_profiler", &["-json", "SPStorageDataType"])
        .map(|text| parse_storage_profile(&parse_system_profiler_json(&text)))
        .unwrap_or_default()
}

fn collect_battery() -> Vec<BatteryInfo> {
    run_command("system_profiler", &["-json", "SPPowerDataType"])
        .map(|text| parse_power_profile(&parse_system_profiler_json(&text)))
        .unwrap_or_default()
}

fn collect_motherboard() -> MotherboardInfo {
    let hardware = run_command("system_profiler", &["-json", "SPHardwareDataType"])
        .map(|text| parse_system_profiler_json(&text))
        .unwrap_or(Value::Null);
    let platform = run_command("system_profiler", &["-json", "SPPlatformDataType"])
        .map(|text| parse_system_profiler_json(&text))
        .unwrap_or(Value::Null);

    let hardware_entry = section_entries(&hardware, "SPHardwareDataType")
        .into_iter()
        .next();
    let platform_entry = section_entries(&platform, "SPPlatformDataType")
        .into_iter()
        .next();

    let model_identifier = hardware_entry
        .and_then(|entry| first_string_field(entry, &["model_identifier", "_name"]))
        .unwrap_or_default();
    let machine_model = hardware_entry
        .and_then(|entry| first_string_field(entry, &["machine_model"]))
        .unwrap_or_default();
    let machine_name = hardware_entry
        .and_then(|entry| first_string_field(entry, &["machine_name"]))
        .unwrap_or_default();
    let chip_type = hardware_entry
        .and_then(|entry| first_string_field(entry, &["chip_type", "cpu_type"]))
        .unwrap_or_default();
    let boot_rom = hardware_entry
        .and_then(|entry| first_string_field(entry, &["boot_rom_version"]))
        .unwrap_or_default();
    let firmware = platform_entry
        .and_then(|entry| {
            first_string_field(entry, &["system_firmware_version", "os_loader_version"])
        })
        .unwrap_or(boot_rom);

    MotherboardInfo {
        manufacturer: "Apple Inc.".to_string(),
        model: if model_identifier.is_empty() {
            machine_model.clone()
        } else {
            model_identifier
        },
        version: machine_model,
        bios_vendor: "Apple Inc.".to_string(),
        bios_version: firmware,
        chipset: chip_type,
        form_factor: machine_name,
        ..MotherboardInfo::default()
    }
}

fn collect_network() -> Vec<NetworkInfo> {
    if let Some(text) = run_command("system_profiler", &["-json", "SPNetworkDataType"]) {
        let networks = parse_network_profile(&parse_system_profiler_json(&text));
        if !networks.is_empty() {
            return networks;
        }
    }

    run_command("ifconfig", &["-a"])
        .map(|text| parse_ifconfig(&text))
        .unwrap_or_default()
}

fn collect_sensors() -> (Vec<FanReading>, Option<f32>, Option<f32>) {
    let Some(text) = run_command(
        "powermetrics",
        &["--samplers", "smc", "-n", "1", "-i", "1000"],
    ) else {
        return (Vec::new(), None, None);
    };

    parse_powermetrics(&text)
}

fn section_entries<'a>(value: &'a Value, key: &str) -> Vec<&'a Value> {
    let target = value.get(key).unwrap_or(value);

    match target {
        Value::Array(entries) => entries.iter().collect(),
        Value::Object(_) => vec![target],
        _ => Vec::new(),
    }
}

fn leaf_entries(entries: Vec<&Value>) -> Vec<&Value> {
    let mut leaves = Vec::new();

    for entry in entries {
        match entry.get("_items").and_then(Value::as_array) {
            Some(items) if !items.is_empty() => leaves.extend(items.iter()),
            _ => leaves.push(entry),
        }
    }

    leaves
}

fn first_object(value: &Value) -> Option<&Value> {
    match value {
        Value::Array(items) => items.iter().find(|item| item.is_object()),
        Value::Object(_) => Some(value),
        _ => None,
    }
}

fn value_to_string(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(text) => text.trim().to_string(),
        Value::Number(number) => number.to_string(),
        Value::Bool(boolean) => boolean.to_string(),
        _ => return None,
    };

    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn value_to_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => parse_first_f64(text),
        Value::Bool(boolean) => Some(if *boolean { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(value_to_string)
}

fn first_string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| string_field(value, key))
}

fn number_field(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(value_to_f64))
}

fn first_bytes_field(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(parse_bytes))
}

fn bool_field(value: &Value, key: &str) -> Option<bool> {
    match value.get(key)? {
        Value::Bool(boolean) => Some(*boolean),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" => Some(false),
            _ => None,
        },
        Value::Number(number) => number.as_i64().map(|number| number != 0),
        _ => None,
    }
}

fn lookup_value<'a>(value: &'a Value, sections: &[&str], keys: &[&str]) -> Option<&'a Value> {
    for key in keys {
        if let Some(found) = value.get(*key) {
            return Some(found);
        }
    }

    for section in sections {
        if let Some(section_value) = value.get(*section).and_then(first_object) {
            for key in keys {
                if let Some(found) = section_value.get(*key) {
                    return Some(found);
                }
            }
        }
    }

    None
}

fn lookup_string(value: &Value, sections: &[&str], keys: &[&str]) -> Option<String> {
    lookup_value(value, sections, keys).and_then(value_to_string)
}

fn lookup_f64(value: &Value, sections: &[&str], keys: &[&str]) -> Option<f64> {
    lookup_value(value, sections, keys).and_then(value_to_f64)
}

fn lookup_bool(value: &Value, sections: &[&str], keys: &[&str]) -> Option<bool> {
    lookup_value(value, sections, keys).and_then(|found| match found {
        Value::Bool(boolean) => Some(*boolean),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" => Some(false),
            _ => None,
        },
        Value::Number(number) => number.as_i64().map(|number| number != 0),
        _ => None,
    })
}

fn battery_info_has_data(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.keys().any(|key| key.starts_with("sppower_battery_"))
                || map.values().any(battery_info_has_data)
        }
        Value::Array(items) => items.iter().any(battery_info_has_data),
        _ => false,
    }
}

fn detect_gpu_vendor(vendor: &str, model: &str) -> String {
    let combined = format!("{vendor} {model}").to_ascii_lowercase();

    if combined.contains("apple") {
        "Apple".to_string()
    } else if combined.contains("amd") || combined.contains("ati ") || combined.contains("radeon") {
        "AMD".to_string()
    } else if combined.contains("nvidia") || combined.contains("geforce") {
        "NVIDIA".to_string()
    } else if combined.contains("intel") {
        "Intel".to_string()
    } else if vendor.is_empty() {
        "Unknown".to_string()
    } else {
        vendor.to_string()
    }
}

fn storage_interface(entry: &Value, medium: &str) -> String {
    if first_string_field(entry, &["spnvme_spec_version", "nvme_spec_version"]).is_some() {
        return "NVMe".to_string();
    }
    if first_string_field(
        entry,
        &[
            "spsata_spec_version",
            "spata_spec_version",
            "sata_spec_version",
        ],
    )
    .is_some()
    {
        return "SATA".to_string();
    }
    if let Some(interface) = first_string_field(
        entry,
        &["device_interface", "interface", "spstorage_interface"],
    ) {
        return interface;
    }

    let medium = medium.to_ascii_lowercase();
    if medium.contains("nvme") {
        "NVMe".to_string()
    } else if medium.contains("sata") {
        "SATA".to_string()
    } else {
        String::new()
    }
}

fn is_rotational(entry: &Value, medium: &str) -> bool {
    if let Some(rotational) = bool_field(entry, "rotational") {
        return rotational;
    }

    let medium = medium.to_ascii_lowercase();
    medium.contains("rotational") || medium.contains("hard disk") || medium.contains("hdd")
}

fn parse_capacity_gb(text: &str) -> Option<f32> {
    let number = parse_first_f64(text)?;
    let lower = text.to_ascii_lowercase();

    let gb = if lower.contains("tib") || lower.contains("tb") {
        number * 1024.0
    } else if lower.contains("gib") || lower.contains("gb") {
        number
    } else if lower.contains("mib") || lower.contains("mb") {
        number / 1024.0
    } else if lower.contains("kib") || lower.contains("kb") {
        number / (1024.0 * 1024.0)
    } else if lower.contains("byte") {
        number / GIB as f64
    } else {
        return None;
    };

    if gb >= 0.0 {
        Some(gb as f32)
    } else {
        None
    }
}

fn parse_bytes(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64().or_else(|| {
            number
                .as_f64()
                .filter(|value| *value >= 0.0)
                .map(|value| value as u64)
        }),
        Value::String(text) => {
            let number = parse_first_f64(text)?;
            if number < 0.0 {
                return None;
            }
            let lower = text.to_ascii_lowercase();
            let multiplier = if lower.contains("tib") || lower.contains("tb") {
                1_000_000_000_000.0
            } else if lower.contains("gib") || lower.contains("gb") {
                1_000_000_000.0
            } else if lower.contains("mib") || lower.contains("mb") {
                1_000_000.0
            } else if lower.contains("kib") || lower.contains("kb") {
                1_000.0
            } else {
                1.0
            };
            Some((number * multiplier) as u64)
        }
        _ => None,
    }
}

fn parse_first_f64(text: &str) -> Option<f64> {
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;

    while index < chars.len() {
        let starts_number = chars[index].is_ascii_digit()
            || (chars[index] == '.'
                && index + 1 < chars.len()
                && chars[index + 1].is_ascii_digit())
            || (chars[index] == '-'
                && index + 1 < chars.len()
                && (chars[index + 1].is_ascii_digit() || chars[index + 1] == '.'));

        if !starts_number {
            index += 1;
            continue;
        }

        let start = index;
        let mut end = index + 1;
        while end < chars.len() && (chars[end].is_ascii_digit() || chars[end] == '.') {
            end += 1;
        }

        let token: String = chars[start..end].iter().collect();
        if let Ok(value) = token.parse::<f64>() {
            return Some(value);
        }
        index = end;
    }

    None
}

fn parse_all_u32(text: &str) -> Vec<u32> {
    let mut numbers = Vec::new();
    let mut current = String::new();

    for character in text.chars() {
        if character.is_ascii_digit() {
            current.push(character);
        } else if !current.is_empty() {
            if let Ok(value) = current.parse::<u32>() {
                numbers.push(value);
            }
            current.clear();
        }
    }

    if !current.is_empty() {
        if let Ok(value) = current.parse::<u32>() {
            numbers.push(value);
        }
    }

    numbers
}

fn parse_first_u32(text: &str) -> Option<u32> {
    parse_all_u32(text).first().copied()
}

fn parse_fan_line(line: &str) -> Option<FanReading> {
    let lower = line.to_ascii_lowercase();
    let rpm_position = lower.find("rpm")?;
    let fan_position = lower.find("fan")?;

    if fan_position > rpm_position {
        return None;
    }
    if fan_position > 0
        && line[..fan_position]
            .chars()
            .next_back()
            .is_some_and(|character| character.is_ascii_alphanumeric())
    {
        return None;
    }

    let label_end = fan_position + 3;
    let label = line[..label_end].trim();
    let after_fan = &line[label_end..];

    let (index, rpm) = if let Some((head, tail)) = after_fan.split_once(':') {
        (parse_first_u32(head), parse_first_u32(tail)?)
    } else {
        let numbers = parse_all_u32(after_fan);
        match numbers.as_slice() {
            [rpm] => (None, *rpm),
            [index, rpm, ..] => (Some(*index), *rpm),
            [] => return None,
        }
    };

    let base = if label.eq_ignore_ascii_case("fan") {
        "Fan".to_string()
    } else {
        label.to_string()
    };
    let name = match index {
        Some(index) => format!("{base} {index}"),
        None => base,
    };

    Some(FanReading {
        name,
        rpm,
        percent: 0.0,
        target_rpm: 0,
        controllable: false,
    })
}

fn parse_temperature_value(line: &str) -> Option<f32> {
    let text = line.split_once(':').map(|(_, value)| value).unwrap_or(line);
    parse_first_f64(text).map(|value| value as f32)
}

fn capacity_mah_to_mwh(value: f64, voltage_mv: Option<f64>) -> u32 {
    if value <= 0.0 {
        return 0;
    }

    match voltage_mv {
        Some(voltage) if voltage > 0.0 => clamp_u32(value * voltage / 1000.0),
        _ => clamp_u32(value),
    }
}

fn clamp_u32(value: f64) -> u32 {
    if value <= 0.0 {
        0
    } else if value >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        value.round() as u32
    }
}

fn battery_temperature_celsius(raw: f64) -> Option<f32> {
    if raw <= 0.0 {
        return None;
    }
    if (0.0..=120.0).contains(&raw) {
        return Some(raw as f32);
    }

    let from_kelvin = raw / 10.0 - 273.15;
    if (-20.0..=120.0).contains(&from_kelvin) {
        return Some(from_kelvin as f32);
    }

    let from_centi = raw / 100.0;
    if (-20.0..=120.0).contains(&from_centi) {
        return Some(from_centi as f32);
    }

    None
}

fn parse_network_profile(value: &Value) -> Vec<NetworkInfo> {
    let entries = section_entries(value, "SPNetworkDataType");
    let mut networks = Vec::new();

    for entry in entries {
        let name = first_string_field(
            entry,
            &["bsd_device_name", "spnetwork_device_name", "_name"],
        )
        .unwrap_or_default();
        if name.is_empty()
            || networks
                .iter()
                .any(|network: &NetworkInfo| network.name == name)
        {
            continue;
        }

        networks.push(NetworkInfo {
            name,
            mac: first_string_field(
                entry,
                &["spnetwork_mac_address", "spnetwork_permanent_mac_address"],
            )
            .unwrap_or_default(),
            speed_mbps: first_speed_mbps(
                entry,
                &[
                    "spnetwork_speed",
                    "spnetwork_link_speed",
                    "spnetwork_media_speed",
                ],
            )
            .unwrap_or(0),
            duplex: first_string_field(entry, &["spnetwork_duplex", "duplex"]).unwrap_or_default(),
            driver: first_string_field(
                entry,
                &[
                    "spnetwork_type",
                    "spnetwork_interface_type",
                    "spnetwork_driver",
                    "spnetwork_media_subtype",
                ],
            )
            .unwrap_or_default(),
            link_up: network_link_up(entry),
            rx_bytes: number_field(entry, &["spnetwork_bytes_received", "rx_bytes"])
                .map(|value| value.max(0.0) as u64)
                .unwrap_or(0),
            tx_bytes: number_field(entry, &["spnetwork_bytes_sent", "tx_bytes"])
                .map(|value| value.max(0.0) as u64)
                .unwrap_or(0),
            ..NetworkInfo::default()
        });
    }

    networks
}

fn first_speed_mbps(value: &Value, keys: &[&str]) -> Option<u32> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(value_to_string)
            .and_then(|text| parse_speed_mbps(&text))
    })
}

fn parse_speed_mbps(text: &str) -> Option<u32> {
    let value = parse_first_f64(text)?;
    if value < 0.0 {
        return None;
    }

    let lower = text.to_ascii_lowercase();
    let mbps = if lower.contains("gbps") || lower.contains("gbit") {
        value * 1000.0
    } else if lower.contains("kbps") {
        value / 1000.0
    } else {
        value
    };

    Some(mbps.round().min(f64::from(u32::MAX)) as u32)
}

fn network_link_up(entry: &Value) -> bool {
    if let Some(status) = first_string_field(
        entry,
        &["spnetwork_status", "spnetwork_link_status", "status"],
    ) {
        let status = status.to_ascii_lowercase();
        if status == "active" || status == "up" || status == "connected" {
            return true;
        }
    }

    ["spnetwork_ipv4", "spnetwork_ipv6"]
        .iter()
        .any(|key| entry.get(*key).is_some_and(network_has_address))
}

fn network_has_address(value: &Value) -> bool {
    for key in [
        "spnetwork_ipv4_addresses",
        "spnetwork_ipv6_addresses",
        "addresses",
    ] {
        if value
            .get(key)
            .and_then(Value::as_array)
            .is_some_and(|addresses| !addresses.is_empty())
        {
            return true;
        }
    }

    value
        .get("spnetwork_ipv4_address")
        .is_some_and(|address| !address.is_null())
        || value
            .get("spnetwork_ipv6_address")
            .is_some_and(|address| !address.is_null())
}

fn parse_ifconfig(text: &str) -> Vec<NetworkInfo> {
    let mut networks = Vec::new();
    let mut current: Option<NetworkInfo> = None;

    for line in text.lines() {
        let indented = line.starts_with(' ') || line.starts_with('\t');

        if !indented {
            if let Some(info) = current.take() {
                if !info.mac.is_empty() {
                    networks.push(info);
                }
            }

            let name = line.split(':').next().unwrap_or("").trim();
            if !name.is_empty() {
                current = Some(NetworkInfo {
                    name: name.to_string(),
                    ..NetworkInfo::default()
                });
            }
            continue;
        }

        let Some(info) = current.as_mut() else {
            continue;
        };
        let trimmed = line.trim();

        if let Some(rest) = trimmed.strip_prefix("ether ") {
            info.mac = rest.split_whitespace().next().unwrap_or("").to_string();
        } else if let Some(rest) = trimmed.strip_prefix("status:") {
            info.link_up = rest.trim() == "active";
        } else if trimmed.starts_with("inet ") || trimmed.starts_with("inet6 ") {
            info.link_up = true;
        }
    }

    if let Some(info) = current {
        if !info.mac.is_empty() {
            networks.push(info);
        }
    }

    networks
}

const GIB: f32 = 1024.0 * 1024.0 * 1024.0;

#[cfg(test)]
mod tests {
    use super::*;

    fn json(payload: &str) -> Value {
        parse_system_profiler_json(payload)
    }

    #[test]
    fn sysctl_parses_keyed_and_bare_values() {
        let text = "hw.ncpu: 8\n\
                    hw.physicalcpu: 4\n\
                    machdep.cpu.brand_string: Apple M1 Pro\n\
                    \n\
                    kern.osversion: 23F79\n";
        let values = parse_sysctl(text);

        assert_eq!(values.get("hw.ncpu").map(String::as_str), Some("8"));
        assert_eq!(values.get("hw.physicalcpu").map(String::as_str), Some("4"));
        assert_eq!(
            values.get("machdep.cpu.brand_string").map(String::as_str),
            Some("Apple M1 Pro")
        );
        assert_eq!(
            values.get("kern.osversion").map(String::as_str),
            Some("23F79")
        );

        let bare = parse_sysctl("8\n");
        assert_eq!(bare.get("").map(String::as_str), Some("8"));
    }

    #[test]
    fn sysctl_keeps_colons_inside_values() {
        let values = parse_sysctl("some.key: value:with:colons\n");
        assert_eq!(
            values.get("some.key").map(String::as_str),
            Some("value:with:colons")
        );
    }

    #[test]
    fn system_profiler_json_handles_bad_input() {
        assert!(parse_system_profiler_json("").is_null());
        assert!(parse_system_profiler_json("{not valid json").is_null());
        assert!(parse_system_profiler_json("{}").is_object());
    }

    #[test]
    fn memory_profile_parses_slots() {
        let payload = r#"
        {
          "SPMemoryDataType": [
            {
              "_name": "BANK 0/DIMM0",
              "dimm_slot": "DIMM0",
              "dimm_size": "16 GB",
              "dimm_type": "DDR4",
              "dimm_speed": "3200 MHz",
              "dimm_manufacturer": "Micron",
              "dimm_part_number": "MTA16ATF2G64AZ-3G2E1",
              "dimm_serial_number": "1A2B3C4D"
            },
            {
              "_name": "BANK 0/DIMM1",
              "dimm_size": "8192 MB",
              "dimm_type": "DDR4",
              "dimm_speed": "3200 MHz",
              "dimm_manufacturer": "Samsung",
              "dimm_part_number": "M378A1K43DB2-CTD",
              "dimm_serial_number": "5E6F7A8B"
            }
          ]
        }
        "#;

        let slots = parse_memory_profile(&json(payload));

        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].slot, "DIMM0");
        assert!((slots[0].size_gb - 16.0).abs() < 0.01);
        assert_eq!(slots[0].memory_type, "DDR4");
        assert_eq!(slots[0].speed_mhz, 3200);
        assert_eq!(slots[0].manufacturer, "Micron");
        assert_eq!(slots[0].part_number, "MTA16ATF2G64AZ-3G2E1");
        assert_eq!(slots[0].serial_number, "1A2B3C4D");

        assert!((slots[1].size_gb - 8.0).abs() < 0.01);
        assert_eq!(slots[1].manufacturer, "Samsung");
    }

    #[test]
    fn memory_profile_accepts_nested_items_and_bare_arrays() {
        let nested = json(
            r#"{"SPMemoryDataType":[{"_items":[{"dimm_size":"32 GB","dimm_type":"DDR5","dimm_speed":"5600 MHz"}]}]}"#,
        );
        let slots = parse_memory_profile(&nested);
        assert_eq!(slots.len(), 1);
        assert!((slots[0].size_gb - 32.0).abs() < 0.01);
        assert_eq!(slots[0].memory_type, "DDR5");

        let bare = json(r#"[{"dimm_size":"4 GB","dimm_type":"DDR3"}]"#);
        let slots = parse_memory_profile(&bare);
        assert_eq!(slots.len(), 1);
        assert!((slots[0].size_gb - 4.0).abs() < 0.01);
    }

    #[test]
    fn memory_profile_ignores_entries_without_spd_fields() {
        let slots = parse_memory_profile(&json(r#"{"SPMemoryDataType":[{}, {"_name":"x"}]}"#));
        assert!(slots.is_empty());
    }

    #[test]
    fn memory_profile_skips_empty_slots() {
        let slots = parse_memory_profile(&json(
            r#"{"SPMemoryDataType":[
                {"dimm_size":"Empty","dimm_type":"Empty","dimm_speed":"Empty"},
                {"dimm_size":"8 GB","dimm_type":"DDR4","dimm_speed":"2400 MHz"}
            ]}"#,
        ));

        assert_eq!(slots.len(), 1);
        assert!((slots[0].size_gb - 8.0).abs() < 0.01);
    }

    #[test]
    fn displays_profile_parses_apple_and_amd_gpus() {
        let payload = r#"
        {
          "SPDisplaysDataType": [
            {
              "_name": "Apple M1 Pro",
              "sppci_model": "Apple M1 Pro",
              "sppci_vendor": "Apple (0x106b)",
              "spdisplays_vram": "Shared",
              "spdisplays_metal": "spdisplays_supported"
            },
            {
              "_name": "Radeon RX 6800",
              "sppci_model": "Radeon RX 6800",
              "sppci_vendor": "AMD (0x1002)",
              "spdisplays_vram": "16 GB",
              "spdisplays_metal": "spdisplays_supported"
            }
          ]
        }
        "#;

        let gpus = parse_displays_profile(&json(payload));

        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].index, 0);
        assert_eq!(gpus[0].vendor, "Apple");
        assert_eq!(gpus[0].model, "Apple M1 Pro");
        assert_eq!(gpus[0].vram_gb, 0.0);
        assert!(gpus[0].driver_version.contains("supported"));

        assert_eq!(gpus[1].index, 1);
        assert_eq!(gpus[1].vendor, "AMD");
        assert!((gpus[1].vram_gb - 16.0).abs() < 0.01);
    }

    #[test]
    fn displays_profile_handles_missing_fields_and_vram_in_mb() {
        let gpus = parse_displays_profile(&json(
            r#"{"SPDisplaysDataType":[{"_name":"Display","spdisplays_vram":"1536 MB"}]}"#,
        ));

        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].vendor, "Unknown");
        assert!((gpus[0].vram_gb - 1.5).abs() < 0.01);
        assert!(gpus[0].driver_version.is_empty());
    }

    #[test]
    fn storage_profile_parses_nvme_and_rotational_disks() {
        let payload = r#"
        {
          "SPStorageDataType": [
            {
              "_name": "APPLE SSD AP0512Q",
              "bsd_name": "disk0",
              "device_model": "APPLE SSD AP0512Q",
              "device_serial": "C0ABC123",
              "device_revision": "1234.100",
              "com.apple.diskmanagement.media.total-size": 500277790720,
              "com.apple.diskmanagement.media.medium-type": "solid state",
              "spnvme_spec_version": "1.4"
            },
            {
              "_name": "ST2000DM008",
              "bsd_name": "disk1",
              "device_model": "ST2000DM008",
              "device_serial": "Z123",
              "spsata_spec_version": "3.0",
              "spstorage_rotation_rate": "7200 RPM",
              "spstorage_medium_type": "Rotational",
              "com.apple.diskmanagement.media.total-size": "2 TB"
            }
          ]
        }
        "#;

        let disks = parse_storage_profile(&json(payload));

        assert_eq!(disks.len(), 2);
        assert_eq!(disks[0].device, "disk0");
        assert_eq!(disks[0].model, "APPLE SSD AP0512Q");
        assert_eq!(disks[0].serial, "C0ABC123");
        assert_eq!(disks[0].interface, "NVMe");
        assert_eq!(disks[0].capacity_bytes, 500_277_790_720);
        assert!(!disks[0].rotational);

        assert_eq!(disks[1].device, "disk1");
        assert_eq!(disks[1].interface, "SATA");
        assert_eq!(disks[1].capacity_bytes, 2_000_000_000_000);
        assert!(disks[1].rotational);
        assert_eq!(disks[1].rpm, 7200);
    }

    #[test]
    fn storage_profile_accepts_nested_items_and_missing_serial() {
        let disks = parse_storage_profile(&json(
            r#"{"SPStorageDataType":[{"_items":[{"bsd_name":"disk2","device_model":"Samsung SSD","size_in_bytes":1000000000}]}]}"#,
        ));

        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].device, "disk2");
        assert_eq!(disks[0].capacity_bytes, 1_000_000_000);
        assert!(disks[0].serial.is_empty());
    }

    #[test]
    fn power_profile_parses_battery_information() {
        let payload = r#"
        {
          "SPPowerDataType": [
            {
              "_name": "spbattery_information",
              "sppower_battery_information": {
                "sppower_battery_charge_info": {
                  "sppower_battery_is_charging": "TRUE",
                  "sppower_battery_fully_charged": "FALSE"
                },
                "sppower_battery_health_info": {
                  "sppower_battery_cycle_count": "123",
                  "sppower_battery_health": "Good",
                  "sppower_battery_health_maximum_capacity": "95%"
                },
                "sppower_battery_model_info": {
                  "sppower_battery_device_name": "bq20z451"
                },
                "sppower_battery_state_of_charge": "80",
                "sppower_battery_voltage": "12000",
                "sppower_battery_amperage": "-1000",
                "sppower_battery_temperature": "3030",
                "sppower_battery_design_capacity": "5500",
                "sppower_battery_full_charge_capacity": "5000",
                "sppower_battery_current_capacity": "4000"
              }
            }
          ]
        }
        "#;

        let batteries = parse_power_profile(&json(payload));

        assert_eq!(batteries.len(), 1);
        let battery = &batteries[0];
        assert_eq!(battery.name, "bq20z451");
        assert_eq!(battery.status, "Good");
        assert_eq!(battery.cycle_count, 123);
        assert_eq!(battery.voltage_mv, 12000);
        assert!((battery.temperature_c - 29.85).abs() < 0.01);
        assert_eq!(battery.capacity_design_mwh, 66000);
        assert_eq!(battery.capacity_full_mwh, 60000);
        assert_eq!(battery.capacity_now_mwh, 48000);
        assert_eq!(battery.charge_rate_mw, 12000);
        assert_eq!(battery.health_percent(), 90);
        assert!((battery.charge_percent() - 80.0).abs() < 0.01);
    }

    #[test]
    fn power_profile_handles_missing_capacity_fields() {
        let batteries = parse_power_profile(&json(
            r#"{"SPPowerDataType":[{"sppower_battery_information":{"sppower_battery_state_of_charge":"55"}}]}"#,
        ));

        assert_eq!(batteries.len(), 1);
        assert_eq!(batteries[0].name, "Battery");
        assert_eq!(batteries[0].status, "");
        assert_eq!(batteries[0].capacity_full_mwh, 0);
        assert_eq!(batteries[0].capacity_now_mwh, 0);
    }

    #[test]
    fn power_profile_ignores_empty_or_non_battery_entries() {
        let batteries = parse_power_profile(&json(
            r#"{"SPPowerDataType":[{},{"_name":"AC Charger","sppower_ac_charger_information":{}}]}"#,
        ));
        assert!(batteries.is_empty());
    }

    #[test]
    fn powermetrics_parses_fans_and_temperatures() {
        let payload = "\
            Machine model: MacBookPro18,3\n\
            SMC sensors:\n\
            Fan: 1200 RPM\n\
            Fan 1: 1500 RPM\n\
            CPU die temperature: 45.50 C\n\
            GPU die temperature: 42.00 C\n";

        let (fans, cpu_temp, gpu_temp) = parse_powermetrics(payload);

        assert_eq!(fans.len(), 2);
        assert_eq!(fans[0].name, "Fan");
        assert_eq!(fans[0].rpm, 1200);
        assert_eq!(fans[1].name, "Fan 1");
        assert_eq!(fans[1].rpm, 1500);
        assert_eq!(cpu_temp, Some(45.5));
        assert_eq!(gpu_temp, Some(42.0));
    }

    #[test]
    fn powermetrics_parses_plist_style_fan_lines() {
        let (fans, cpu_temp, gpu_temp) =
            parse_powermetrics("Fan0 2000 RPM\nCPU die temperature: 0.00 C\n");

        assert_eq!(fans.len(), 1);
        assert_eq!(fans[0].name, "Fan 0");
        assert_eq!(fans[0].rpm, 2000);
        assert_eq!(cpu_temp, Some(0.0));
        assert_eq!(gpu_temp, None);
    }

    #[test]
    fn powermetrics_handles_empty_and_malformed_input() {
        let (fans, cpu_temp, gpu_temp) = parse_powermetrics("");
        assert!(fans.is_empty());
        assert_eq!(cpu_temp, None);
        assert_eq!(gpu_temp, None);

        let (fans, cpu_temp, gpu_temp) =
            parse_powermetrics("Fan: RPM\nFan without a reading\nno sensors here\n");
        assert!(fans.is_empty());
        assert_eq!(cpu_temp, None);
        assert_eq!(gpu_temp, None);
    }

    #[test]
    fn network_profile_parses_interface_details() {
        let networks = parse_network_profile(&json(
            r#"{"SPNetworkDataType":[{"_name":"Wi-Fi","bsd_device_name":"en0","spnetwork_mac_address":"aa:bb:cc:dd:ee:ff","spnetwork_speed":"1300","spnetwork_type":"AirPort","spnetwork_ipv4":{"spnetwork_ipv4_addresses":[{"spnetwork_ipv4_address":"192.168.1.2"}]}}]}"#,
        ));

        assert_eq!(networks.len(), 1);
        assert_eq!(networks[0].name, "en0");
        assert_eq!(networks[0].mac, "aa:bb:cc:dd:ee:ff");
        assert_eq!(networks[0].speed_mbps, 1300);
        assert_eq!(networks[0].driver, "AirPort");
        assert!(networks[0].link_up);
    }

    #[test]
    fn ifconfig_fallback_parses_mac_and_link_state() {
        let payload = "\
            en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n\
            \tether aa:bb:cc:dd:ee:ff\n\
            \tinet 192.168.1.2 netmask 0xffffff00 broadcast 192.168.1.255\n\
            \tstatus: active\n\
            utun0: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1380\n\
            \tinet6 fe80::1%utun0 prefixlen 64 scopeid 0x9\n";

        let networks = parse_ifconfig(payload);

        assert_eq!(networks.len(), 1);
        assert_eq!(networks[0].name, "en0");
        assert_eq!(networks[0].mac, "aa:bb:cc:dd:ee:ff");
        assert!(networks[0].link_up);
    }

    #[test]
    fn capacity_and_byte_helpers_parse_common_units() {
        assert_eq!(parse_capacity_gb("16 GB"), Some(16.0));
        assert_eq!(parse_capacity_gb("8192 MB"), Some(8.0));
        assert_eq!(parse_capacity_gb("1 TB"), Some(1024.0));
        assert_eq!(parse_capacity_gb("Shared"), None);

        assert_eq!(
            parse_bytes(&serde_json::Value::String("2 TB".to_string())),
            Some(2_000_000_000_000)
        );
        assert_eq!(parse_bytes(&serde_json::Value::from(1234)), Some(1234));
        assert_eq!(parse_bytes(&serde_json::Value::Null), None);
    }

    #[test]
    fn battery_temperature_supports_apple_units() {
        assert_eq!(battery_temperature_celsius(3030.0), Some(29.85));
        assert_eq!(battery_temperature_celsius(45.0), Some(45.0));
        assert_eq!(battery_temperature_celsius(-1.0), None);
    }
}
