// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Windows hardware and sensor collection (WMI + LibreHardwareMonitor).
//!
//! Inventory comes from `Get-CimInstance` (with a `Get-WmiObject` fallback)
//! executed through PowerShell. Optional deep sensors come from the
//! LibreHardwareMonitor WMI namespace, NVIDIA GPUs from `nvidia-smi`, and
//! storage health from `smartctl`. Every external query is best-effort: a
//! missing tool or a failed class returns an empty result and collection
//! continues. Subprocesses are bounded by a polling timeout, so a wedged
//! provider cannot block the agent indefinitely.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::Value;

use crate::{
    BatteryInfo, CpuCoreInfo, CpuPackageInfo, FanReading, GpuInfo, HardwareCollector,
    HardwareSensor, HardwareSnapshot, MemorySlotInfo, MemoryTiming, MotherboardInfo, NetworkInfo,
    NvmeData, PcieDevice, SmartAttribute, SmartData, StorageInfo, VoltageReading,
};

const CIMV2_NAMESPACE: &str = "root\\cimv2";
const LHM_NAMESPACE: &str = "root\\LibreHardwareMonitor";

const WMI_TIMEOUT: Duration = Duration::from_secs(30);
const SENSOR_TIMEOUT: Duration = Duration::from_secs(15);
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);

const QUERY_CPU: &str = "SELECT Name, NumberOfCores, NumberOfLogicalProcessors, \
     MaxClockSpeed, CurrentClockSpeed, SocketDesignation FROM Win32_Processor";
const QUERY_MEMORY: &str = "SELECT DeviceLocator, BankLabel, Capacity, Speed, \
     ConfiguredClockSpeed, MemoryType, SMBIOSMemoryType, Manufacturer, PartNumber, \
     SerialNumber, FormFactor, ConfiguredVoltage FROM Win32_PhysicalMemory";
const QUERY_VIDEO: &str = "SELECT Name, AdapterRAM, DriverVersion FROM Win32_VideoController";
const QUERY_DISK: &str = "SELECT DeviceID, Model, SerialNumber, FirmwareRevision, Size, \
     MediaType, InterfaceType FROM Win32_DiskDrive";
const QUERY_BASEBOARD: &str = "SELECT Manufacturer, Product, Version FROM Win32_BaseBoard";
const QUERY_BIOS: &str = "SELECT Manufacturer, SMBIOSBIOSVersion, ReleaseDate FROM Win32_BIOS";
const QUERY_BATTERY: &str = "SELECT Name, BatteryStatus, DesignCapacity, FullChargeCapacity, \
     RemainingCapacity, Chemistry, Voltage FROM Win32_Battery";
const QUERY_NETWORK: &str = "SELECT Name, NetConnectionID, MACAddress, Speed, AdapterType, \
     NetConnectionStatus, NetEnabled FROM Win32_NetworkAdapter WHERE PhysicalAdapter=True";
const QUERY_PCIE: &str = "SELECT Name, PNPDeviceID, Manufacturer, Service, PNPClass \
     FROM Win32_PnPEntity WHERE PNPDeviceID LIKE 'PCI%'";
const QUERY_LHM_SENSORS: &str = "SELECT Name, SensorType, Value, Min, Max, Parent FROM Sensor";

const NVIDIA_SMI_ARGS: [&str; 2] = [
    "--query-gpu=index,name,memory.total,driver_version,temperature.gpu,utilization.gpu,power.draw,clocks.current.graphics,clocks.current.memory",
    "--format=csv,noheader,nounits",
];

const BYTES_PER_GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Windows collector using WMI Win32_* classes and LibreHardwareMonitor.
pub struct WindowsHardwareCollector;

impl WindowsHardwareCollector {
    /// Create a Windows collector.
    pub fn new() -> Self {
        Self
    }

    fn query(&self, query: &str, namespace: &str) -> String {
        self.query_with_timeout(query, namespace, WMI_TIMEOUT)
    }

    fn query_with_timeout(&self, query: &str, namespace: &str, timeout: Duration) -> String {
        let script = build_wmi_script(query, namespace);
        match run_command_bounded(
            "powershell.exe",
            &["-NonInteractive", "-NoProfile", "-Command", script.as_str()],
            timeout,
        ) {
            Some((_, stdout)) => stdout,
            None => String::new(),
        }
    }

    fn collect_gpus(&self) -> Vec<GpuInfo> {
        if let Some((true, stdout)) =
            run_command_bounded("nvidia-smi", &NVIDIA_SMI_ARGS, TOOL_TIMEOUT)
        {
            let gpus = parse_nvidia_smi(&stdout);
            if !gpus.is_empty() {
                return gpus;
            }
        }

        parse_win32_video_controller(&self.query(QUERY_VIDEO, CIMV2_NAMESPACE))
    }

    fn collect_storage(&self) -> Vec<StorageInfo> {
        let mut devices = parse_win32_diskdrive(&self.query(QUERY_DISK, CIMV2_NAMESPACE));
        for device in &mut devices {
            if let Some(smart) = collect_smart(&device.device) {
                device.temperature_c = smart.temperature_c;
                device.smart = Some(smart);
            }
        }
        devices
    }

    fn collect_motherboard(&self) -> MotherboardInfo {
        let mut board = MotherboardInfo::default();

        if let Some(row) = parse_wmi_rows(&self.query(QUERY_BASEBOARD, CIMV2_NAMESPACE)).first() {
            board.manufacturer = json_str(row, "Manufacturer").unwrap_or_default();
            board.model = json_str(row, "Product").unwrap_or_default();
            board.version = json_str(row, "Version").unwrap_or_default();
        }

        if let Some(row) = parse_wmi_rows(&self.query(QUERY_BIOS, CIMV2_NAMESPACE)).first() {
            board.bios_vendor = json_str(row, "Manufacturer").unwrap_or_default();
            board.bios_version = json_str(row, "SMBIOSBIOSVersion").unwrap_or_default();
            board.bios_date = json_str(row, "ReleaseDate")
                .map(|date| format_cim_date(&date))
                .unwrap_or_default();
        }

        board
    }

    fn collect_batteries(&self) -> Vec<BatteryInfo> {
        parse_wmi_rows(&self.query(QUERY_BATTERY, CIMV2_NAMESPACE))
            .iter()
            .map(|row| BatteryInfo {
                name: json_str(row, "Name").unwrap_or_else(|| "Battery".to_string()),
                status: battery_status_name(json_u32(row, "BatteryStatus").unwrap_or(0))
                    .to_string(),
                capacity_design_mwh: json_u32(row, "DesignCapacity").unwrap_or(0),
                capacity_full_mwh: json_u32(row, "FullChargeCapacity").unwrap_or(0),
                capacity_now_mwh: json_u32(row, "RemainingCapacity").unwrap_or(0),
                voltage_mv: json_u32(row, "Voltage").unwrap_or(0),
                technology: battery_chemistry_name(json_u32(row, "Chemistry").unwrap_or(0))
                    .to_string(),
                ..BatteryInfo::default()
            })
            .collect()
    }

    fn collect_network(&self) -> Vec<NetworkInfo> {
        parse_wmi_rows(&self.query(QUERY_NETWORK, CIMV2_NAMESPACE))
            .iter()
            .map(|row| {
                let speed_bps = json_u64(row, "Speed").unwrap_or(0);
                NetworkInfo {
                    name: json_str(row, "NetConnectionID")
                        .or_else(|| json_str(row, "Name"))
                        .unwrap_or_default(),
                    mac: json_str(row, "MACAddress").unwrap_or_default(),
                    speed_mbps: (speed_bps / 1_000_000).min(u64::from(u32::MAX)) as u32,
                    link_up: json_u32(row, "NetConnectionStatus") == Some(2)
                        || json_bool(row, "NetEnabled").unwrap_or(false),
                    ..NetworkInfo::default()
                }
            })
            .collect()
    }

    fn collect_pcie(&self) -> Vec<PcieDevice> {
        let mut devices = Vec::new();

        for row in &parse_wmi_rows(&self.query(QUERY_PCIE, CIMV2_NAMESPACE)) {
            let pnp_id = json_str(row, "PNPDeviceID").unwrap_or_default();
            if !pnp_id.to_ascii_uppercase().starts_with("PCI\\") {
                continue;
            }

            devices.push(PcieDevice {
                class_name: json_str(row, "PNPClass").unwrap_or_default(),
                vendor: json_str(row, "Manufacturer").unwrap_or_default(),
                device: json_str(row, "Name").unwrap_or_default(),
                driver: json_str(row, "Service").unwrap_or_default(),
                ..PcieDevice::default()
            });
        }

        if devices.is_empty() {
            tracing::warn!("PCIe topology unavailable: Win32_PnPEntity returned no PCI devices");
        }

        devices
    }
}

impl Default for WindowsHardwareCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl HardwareCollector for WindowsHardwareCollector {
    fn collect(&mut self) -> Result<HardwareSnapshot> {
        let mut snapshot = HardwareSnapshot {
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64(),
            hostname: hostname::get()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "unknown".to_string()),
            os: "Windows".to_string(),
            ..HardwareSnapshot::default()
        };

        snapshot.os_version = parse_wmi_rows(
            &self.query("SELECT Version FROM Win32_OperatingSystem", CIMV2_NAMESPACE),
        )
        .first()
        .and_then(|row| json_str(row, "Version"))
        .unwrap_or_default();

        snapshot.cpu = parse_win32_processor(&self.query(QUERY_CPU, CIMV2_NAMESPACE));
        snapshot.memory_slots = parse_win32_memory(&self.query(QUERY_MEMORY, CIMV2_NAMESPACE));
        snapshot.total_memory_gb = snapshot.memory_slots.iter().map(|slot| slot.size_gb).sum();
        snapshot.gpus = self.collect_gpus();
        snapshot.storage = self.collect_storage();
        snapshot.motherboard = self.collect_motherboard();
        snapshot.batteries = self.collect_batteries();
        snapshot.network = self.collect_network();
        snapshot.pcie_devices = self.collect_pcie();

        let sensors = parse_wmi_rows(&self.query_with_timeout(
            QUERY_LHM_SENSORS,
            LHM_NAMESPACE,
            SENSOR_TIMEOUT,
        ));
        if sensors.is_empty() {
            tracing::debug!("LibreHardwareMonitor WMI sensors unavailable");
        }
        apply_lhm_sensors(&mut snapshot, &sensors);

        Ok(snapshot)
    }
}

// ── Parsing helpers ───────────────────────────────────────────────────────────

/// Parse PowerShell/CIM JSON into a list of row objects.
///
/// Handles a JSON array, a single object, `null`, a UTF-8 BOM, and empty or
/// malformed output. Non-object array entries are ignored.
pub fn parse_wmi_rows(payload: &str) -> Vec<Value> {
    let payload = payload.trim().trim_start_matches('\u{feff}').trim();
    if payload.is_empty() {
        return Vec::new();
    }

    match serde_json::from_str::<Value>(payload) {
        Ok(Value::Array(items)) => items.into_iter().filter(Value::is_object).collect(),
        Ok(Value::Object(object)) => vec![Value::Object(object)],
        _ => Vec::new(),
    }
}

/// Parse `Win32_Processor` output. Only the first socket is described; the
/// logical-core list always starts at index zero.
pub fn parse_win32_processor(payload: &str) -> CpuPackageInfo {
    let rows = parse_wmi_rows(payload);
    let Some(row) = rows.first() else {
        return CpuPackageInfo::default();
    };

    let brand = json_str(row, "Name").unwrap_or_default();
    let physical_cores = json_u32(row, "NumberOfCores").unwrap_or(0);
    let logical_cores = json_u32(row, "NumberOfLogicalProcessors").unwrap_or(0);
    let max_clock = json_f64(row, "MaxClockSpeed").unwrap_or(0.0) as f32;
    let current_clock = json_f64(row, "CurrentClockSpeed").unwrap_or(0.0) as f32;

    let cores = (0..logical_cores)
        .map(|index| CpuCoreInfo {
            index,
            clock_mhz: current_clock,
            ..CpuCoreInfo::default()
        })
        .collect();

    CpuPackageInfo {
        brand: brand.clone(),
        model: brand,
        socket: json_str(row, "SocketDesignation").unwrap_or_default(),
        sockets: u32::try_from(rows.len()).unwrap_or(u32::MAX),
        physical_cores,
        logical_cores,
        base_clock_mhz: max_clock,
        cores,
        ..CpuPackageInfo::default()
    }
}

/// Parse `Win32_PhysicalMemory` output into memory-slot records.
pub fn parse_win32_memory(payload: &str) -> Vec<MemorySlotInfo> {
    parse_wmi_rows(payload)
        .iter()
        .filter_map(|row| {
            let capacity = json_u64(row, "Capacity").unwrap_or(0);
            if capacity == 0 {
                return None;
            }

            let smbios_type = json_u64(row, "SMBIOSMemoryType").unwrap_or(0);
            let memory_type = if smbios_type != 0 {
                smbios_type
            } else {
                json_u64(row, "MemoryType").unwrap_or(0)
            };
            let form_factor = json_u64(row, "FormFactor").unwrap_or(0);
            let voltage_mv = json_u32(row, "ConfiguredVoltage").unwrap_or(0);
            let bank = json_str(row, "BankLabel").unwrap_or_default();
            let slot = json_str(row, "DeviceLocator").unwrap_or_default();

            Some(MemorySlotInfo {
                channel: derive_memory_channel(&bank, &slot),
                slot,
                bank,
                size_gb: (capacity as f64 / BYTES_PER_GIB) as f32,
                memory_type: memory_type_name(memory_type),
                speed_mhz: json_u32(row, "Speed").unwrap_or(0),
                configured_speed_mhz: json_u32(row, "ConfiguredClockSpeed").unwrap_or(0),
                manufacturer: json_str(row, "Manufacturer").unwrap_or_default(),
                part_number: json_str(row, "PartNumber").unwrap_or_default(),
                serial_number: json_str(row, "SerialNumber").unwrap_or_default(),
                form_factor: form_factor_name(form_factor),
                timing: MemoryTiming {
                    voltage_v: voltage_mv as f32 / 1000.0,
                    ..MemoryTiming::default()
                },
                ..MemorySlotInfo::default()
            })
        })
        .collect()
}

/// Parse `nvidia-smi --query-gpu ... --format=csv,noheader,nounits` output.
pub fn parse_nvidia_smi(payload: &str) -> Vec<GpuInfo> {
    let mut gpus = Vec::new();

    for (fallback_index, line) in payload.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.to_ascii_lowercase().starts_with("index") {
            continue;
        }

        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        // index, name, memory.total, driver_version, temperature.gpu,
        // utilization.gpu, power.draw, clocks.current.graphics,
        // clocks.current.memory.
        if fields.len() < 9 {
            continue;
        }

        // A GPU name may itself contain commas, so the last seven columns are
        // always the fixed numeric/driver tail.
        let tail_start = fields.len() - 7;
        let model = fields[1..tail_start].join(", ");
        if model.is_empty() {
            continue;
        }

        gpus.push(GpuInfo {
            index: parse_u32_field(fields[0])
                .unwrap_or_else(|| u32::try_from(fallback_index).unwrap_or(0)),
            vendor: gpu_vendor(&model).to_string(),
            model,
            vram_gb: (parse_f64_field(fields[tail_start]).unwrap_or(0.0) / 1024.0) as f32,
            driver_version: fields[tail_start + 1].to_string(),
            temperature_c: parse_f64_field(fields[tail_start + 2]).unwrap_or(0.0) as f32,
            utilization_percent: parse_f64_field(fields[tail_start + 3]).unwrap_or(0.0) as f32,
            power_w: parse_f64_field(fields[tail_start + 4]).unwrap_or(0.0) as f32,
            core_clock_mhz: parse_f64_field(fields[tail_start + 5]).unwrap_or(0.0) as f32,
            memory_clock_mhz: parse_f64_field(fields[tail_start + 6]).unwrap_or(0.0) as f32,
            ..GpuInfo::default()
        });
    }

    gpus
}

/// Parse `Win32_DiskDrive` output into storage records.
pub fn parse_win32_diskdrive(payload: &str) -> Vec<StorageInfo> {
    parse_wmi_rows(payload)
        .iter()
        .map(|row| {
            let model = json_str(row, "Model").unwrap_or_default();
            let media = json_str(row, "MediaType").unwrap_or_default();
            let interface = json_str(row, "InterfaceType").unwrap_or_default();
            let is_solid_state = is_solid_state(&model, &media, &interface);

            StorageInfo {
                device: json_str(row, "DeviceID").unwrap_or_default(),
                model,
                serial: json_str(row, "SerialNumber").unwrap_or_default(),
                firmware: json_str(row, "FirmwareRevision").unwrap_or_default(),
                interface,
                capacity_bytes: json_u64(row, "Size").unwrap_or(0),
                rotational: !is_solid_state,
                ..StorageInfo::default()
            }
        })
        .collect()
}

// ── LibreHardwareMonitor mapping ──────────────────────────────────────────────

/// Map LibreHardwareMonitor `Sensor` rows onto the snapshot.
///
/// Sensors with a missing, non-numeric, or non-finite `Value` are skipped.
pub fn apply_lhm_sensors(snapshot: &mut HardwareSnapshot, sensors: &[Value]) {
    let mut fan_controls: Vec<(String, f32)> = Vec::new();

    for sensor in sensors {
        let name = json_str(sensor, "Name").unwrap_or_default();
        let sensor_type = json_str(sensor, "SensorType")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let parent = json_str(sensor, "Parent").unwrap_or_default();
        let Some(value) = json_f64(sensor, "Value") else {
            continue;
        };
        if name.is_empty() || sensor_type.is_empty() {
            continue;
        }

        let unit = match sensor_type.as_str() {
            "temperature" => "C",
            "voltage" => "V",
            "fan" => "RPM",
            "power" => "W",
            "clock" => "MHz",
            "load" => "%",
            "throughput" => "MB/s",
            _ => "",
        };
        let id = json_str(sensor, "Identifier")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| {
                format!(
                    "lhm:{}:{name}",
                    if parent.is_empty() {
                        "unknown"
                    } else {
                        &parent
                    }
                )
            });
        snapshot.sensors.push(HardwareSensor {
            id,
            chip: parent.clone(),
            label: name.clone(),
            kind: sensor_type.clone(),
            value: value as f32,
            unit: unit.to_string(),
            min: json_f64(sensor, "Min").map(|value| value as f32),
            max: json_f64(sensor, "Max").map(|value| value as f32),
        });

        let lower = name.to_ascii_lowercase();
        match sensor_type.as_str() {
            "temperature" => apply_temperature(snapshot, &lower, &parent, value as f32),
            "clock" => apply_clock(snapshot, &lower, &parent, value as f32),
            "voltage" => apply_voltage(snapshot, &name, &lower, value as f32, sensor),
            "fan" => apply_fan(snapshot, &name, &lower, &parent, value as f32),
            "control" if lower.contains("fan") => {
                fan_controls.push((name, value as f32));
            }
            "control" => {}
            "power" => apply_power(snapshot, &lower, &parent, value as f32),
            "load" => apply_load(snapshot, &lower, &parent, value as f32),
            "throughput" => apply_throughput(snapshot, &lower, &parent, value as f32),
            "smalldata" => apply_smalldata(snapshot, &lower, &parent, value as f32),
            _ => {}
        }
    }

    for (name, percent) in fan_controls {
        let lower = name.to_ascii_lowercase();
        if lower.contains("gpu") {
            apply_to_gpus(&mut snapshot.gpus, "", |gpu| gpu.fan_percent = percent);
        }
        for fan in &mut snapshot.fans {
            let fan_name = fan.name.to_ascii_lowercase();
            if fan_name == lower || fan_name.contains(&lower) || lower.contains(&fan_name) {
                fan.percent = percent;
                fan.controllable = true;
            }
        }
    }
}

fn apply_temperature(snapshot: &mut HardwareSnapshot, lower: &str, parent: &str, value: f32) {
    if !lower.contains("gpu") {
        if let Some(index) = parse_core_index(lower) {
            if let Some(core) = snapshot.cpu.cores.get_mut(index) {
                core.temperature_c = value;
                return;
            }
        }
    }

    if lower.contains("package")
        || lower.contains("tctl")
        || lower.contains("tdie")
        || (lower.contains("cpu") && (lower.contains("die") || lower.contains("temperature")))
    {
        snapshot.cpu.package_temp_c = snapshot.cpu.package_temp_c.max(value);
        return;
    }

    if lower.contains("gpu") {
        if lower.contains("hot spot") || lower.contains("hotspot") {
            apply_to_gpus(&mut snapshot.gpus, parent, |gpu| {
                gpu.hotspot_temp_c = value;
            });
        } else if lower.contains("memory junction")
            || lower.contains("vram")
            || lower.contains("memory")
        {
            apply_to_gpus(&mut snapshot.gpus, parent, |gpu| {
                gpu.vram_temp_c = value;
            });
        } else {
            apply_to_gpus(&mut snapshot.gpus, parent, |gpu| {
                gpu.temperature_c = value;
            });
        }
        return;
    }

    if lower.contains("vrm") {
        snapshot.motherboard.vrm_temp_c = snapshot.motherboard.vrm_temp_c.max(value);
    } else if lower.contains("pch") {
        snapshot.motherboard.pch_temp_c = snapshot.motherboard.pch_temp_c.max(value);
    } else if lower.contains("ambient") || lower.contains("motherboard") {
        snapshot.motherboard.ambient_temp_c = value;
    }
}

fn apply_clock(snapshot: &mut HardwareSnapshot, lower: &str, parent: &str, value: f32) {
    if !lower.contains("gpu") {
        if let Some(index) = parse_core_index(lower) {
            if let Some(core) = snapshot.cpu.cores.get_mut(index) {
                core.clock_mhz = value;
                return;
            }
        }
    }

    if !lower.contains("gpu") {
        return;
    }

    if lower.contains("core") || lower.contains("graphics") || lower.contains("shader") {
        if lower.contains("shader") {
            apply_to_gpus(&mut snapshot.gpus, parent, |gpu| {
                gpu.shader_clock_mhz = value;
            });
        } else {
            apply_to_gpus(&mut snapshot.gpus, parent, |gpu| {
                gpu.core_clock_mhz = value;
            });
        }
    } else if lower.contains("memory") {
        apply_to_gpus(&mut snapshot.gpus, parent, |gpu| {
            gpu.memory_clock_mhz = value;
        });
    }
}

fn apply_voltage(
    snapshot: &mut HardwareSnapshot,
    name: &str,
    lower: &str,
    value: f32,
    sensor: &Value,
) {
    if !lower.contains("gpu")
        && (lower.contains("vcore")
            || lower.contains("cpu core")
            || lower.contains("core voltage")
            || lower.contains("package"))
    {
        snapshot.cpu.core_voltage_v = value;
        if let Some(index) = parse_core_index(lower) {
            if let Some(core) = snapshot.cpu.cores.get_mut(index) {
                core.voltage_v = value;
            }
        }
        return;
    }

    if lower.contains("gpu") {
        return;
    }

    snapshot.voltages.push(VoltageReading {
        name: name.to_string(),
        voltage_v: value,
        min_v: json_f64(sensor, "Min").unwrap_or(0.0) as f32,
        max_v: json_f64(sensor, "Max").unwrap_or(0.0) as f32,
        nominal_v: 0.0,
    });
}

fn apply_fan(snapshot: &mut HardwareSnapshot, name: &str, lower: &str, parent: &str, value: f32) {
    let rpm = value.max(0.0).round() as u32;
    if lower.contains("gpu") {
        apply_to_gpus(&mut snapshot.gpus, parent, |gpu| gpu.fan_rpm = rpm);
    }

    snapshot.fans.push(FanReading {
        name: name.to_string(),
        rpm,
        percent: 0.0,
        target_rpm: 0,
        controllable: false,
    });
}

fn apply_power(snapshot: &mut HardwareSnapshot, lower: &str, parent: &str, value: f32) {
    if lower.contains("gpu") {
        apply_to_gpus(&mut snapshot.gpus, parent, |gpu| gpu.power_w = value);
        return;
    }

    if let Some(index) = parse_core_index(lower) {
        if let Some(core) = snapshot.cpu.cores.get_mut(index) {
            core.power_w = value;
            return;
        }
    }

    if lower.contains("uncore") {
        snapshot.cpu.uncore_power_w = value;
    } else if lower.contains("dram") {
        snapshot.cpu.dram_power_w = value;
    } else if lower.contains("package") {
        snapshot.cpu.package_power_w = value;
    } else if lower.contains("core") {
        snapshot.cpu.core_power_w = value;
    }
}

fn apply_load(snapshot: &mut HardwareSnapshot, lower: &str, parent: &str, value: f32) {
    if !lower.contains("gpu") {
        if let Some(index) = parse_core_index(lower) {
            if let Some(core) = snapshot.cpu.cores.get_mut(index) {
                core.usage_percent = value;
                return;
            }
        }
        return;
    }

    apply_to_gpus(&mut snapshot.gpus, parent, |gpu| {
        gpu.utilization_percent = value;
    });
}

fn apply_throughput(snapshot: &mut HardwareSnapshot, lower: &str, parent: &str, value: f32) {
    if !lower.contains("gpu") {
        return;
    }

    if lower.contains("memory") && lower.contains("read") {
        // LibreHardwareMonitor reports GPU memory throughput in MB/s.
        apply_to_gpus(&mut snapshot.gpus, parent, |gpu| {
            gpu.memory_bandwidth_gbs = value / 1024.0;
        });
    } else if lower.contains("pcie") {
        apply_to_gpus(&mut snapshot.gpus, parent, |gpu| {
            gpu.pcie_bandwidth_mbs = value;
        });
    }
}

fn apply_smalldata(snapshot: &mut HardwareSnapshot, lower: &str, parent: &str, value: f32) {
    if lower.contains("gpu") && lower.contains("memory") && lower.contains("used") {
        // LibreHardwareMonitor reports GPU memory usage in MB.
        apply_to_gpus(&mut snapshot.gpus, parent, |gpu| {
            gpu.vram_used_gb = value / 1024.0;
        });
    }
}

fn apply_to_gpus(gpus: &mut [GpuInfo], parent: &str, mut apply: impl FnMut(&mut GpuInfo)) {
    if gpus.is_empty() {
        return;
    }

    let parent_lower = parent.to_ascii_lowercase();
    let mut matched = false;
    if !parent_lower.is_empty() {
        for gpu in gpus.iter_mut() {
            let model_lower = gpu.model.to_ascii_lowercase();
            if !model_lower.is_empty()
                && (parent_lower.contains(&model_lower) || model_lower.contains(&parent_lower))
            {
                apply(gpu);
                matched = true;
            }
        }
    }

    if !matched {
        for gpu in gpus.iter_mut() {
            apply(gpu);
        }
    }
}

fn parse_core_index(name: &str) -> Option<usize> {
    for marker in ["core #", "core "] {
        if let Some(position) = name.find(marker) {
            let rest = &name[position + marker.len()..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(number) = digits.parse::<usize>() {
                if number > 0 {
                    return Some(number - 1);
                }
            }
        }
    }
    None
}

// ── Video-controller fallback ─────────────────────────────────────────────────

fn parse_win32_video_controller(payload: &str) -> Vec<GpuInfo> {
    parse_wmi_rows(payload)
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let model = json_str(row, "Name").unwrap_or_default();
            let adapter_ram = json_u64(row, "AdapterRAM").unwrap_or(0);
            let vram_gb = if adapter_ram > 0 && adapter_ram < u64::from(u32::MAX) {
                (adapter_ram as f64 / BYTES_PER_GIB) as f32
            } else {
                0.0
            };

            GpuInfo {
                index: u32::try_from(index).unwrap_or(u32::MAX),
                vendor: gpu_vendor(&model).to_string(),
                model,
                vram_gb,
                driver_version: json_str(row, "DriverVersion").unwrap_or_default(),
                ..GpuInfo::default()
            }
        })
        .collect()
}

// ── SMART ─────────────────────────────────────────────────────────────────────

fn collect_smart(device: &str) -> Option<SmartData> {
    if device.is_empty() {
        return None;
    }

    let (_, stdout) = run_command_bounded("smartctl", &["-x", "--json=c", device], TOOL_TIMEOUT)?;
    let data: Value = serde_json::from_str(&stdout).ok()?;
    parse_smart_json(&data)
}

fn parse_smart_json(data: &Value) -> Option<SmartData> {
    let mut smart = SmartData::default();
    let mut recognized = false;

    if let Some(passed) = data
        .pointer("/smart_status/passed")
        .and_then(Value::as_bool)
    {
        smart.overall_status = if passed { "PASSED" } else { "FAILED" }.to_string();
        recognized = true;
    }

    if let Some(hours) = data.pointer("/power_on_time/hours").and_then(Value::as_u64) {
        smart.power_on_hours = hours;
        recognized = true;
    }

    if let Some(current) = data.pointer("/temperature/current").and_then(Value::as_f64) {
        if current.is_finite() {
            smart.temperature_c = current as f32;
            recognized = true;
        }
    }

    if let Some(result) = data
        .pointer("/ata_smart_data/self_test/status/string")
        .and_then(Value::as_str)
    {
        smart.self_test_result = result.to_string();
    }

    if let Some(attributes) = data
        .pointer("/ata_smart_attributes/table")
        .and_then(Value::as_array)
    {
        recognized = true;
        for attribute in attributes {
            let id = attribute.get("id").and_then(Value::as_u64).unwrap_or(0) as u32;
            let value = attribute.get("value").and_then(Value::as_u64).unwrap_or(0) as u32;
            let worst = attribute.get("worst").and_then(Value::as_u64).unwrap_or(0) as u32;
            let threshold = attribute.get("thresh").and_then(Value::as_u64).unwrap_or(0) as u32;
            let raw = attribute
                .get("raw")
                .and_then(|raw| raw.get("value"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let flags = attribute
                .get("flags")
                .and_then(|flags| flags.get("value"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            let pre_fail = attribute
                .get("flags")
                .and_then(|flags| flags.get("prefailure"))
                .and_then(Value::as_bool)
                .unwrap_or(false);

            smart.attributes.push(SmartAttribute {
                id,
                name: attribute
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                value,
                worst,
                raw,
                threshold,
                flags,
                pre_fail,
                failing: value != 0 && value <= threshold && pre_fail,
            });

            match id {
                4 => smart.start_stop_count = raw,
                5 => smart.reallocated_sectors = raw,
                197 => smart.pending_sectors = raw,
                198 => smart.uncorrectable_errors = raw,
                _ => {}
            }
        }
    }

    let penalty = (smart.reallocated_sectors * 2
        + smart.pending_sectors * 2
        + smart.uncorrectable_errors * 5) as u32;
    smart.health_percent = 100u32.saturating_sub(penalty.min(100));

    if let Some(nvme) = data.get("nvme_smart_health_information_log") {
        recognized = true;
        let nvme_power_on_hours = nvme
            .get("power_on_hours")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if smart.power_on_hours == 0 {
            smart.power_on_hours = nvme_power_on_hours;
        }

        let nvme_data = NvmeData {
            temperature_c: smart.temperature_c,
            available_spare_percent: nvme
                .get("available_spare")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
            available_spare_threshold_percent: nvme
                .get("available_spare_threshold")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
            percentage_used: nvme
                .get("percentage_used")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
            data_units_written: nvme
                .get("data_units_written")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            data_units_read: nvme
                .get("data_units_read")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            host_write_commands: nvme
                .get("host_write_commands")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            host_read_commands: nvme
                .get("host_read_commands")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            media_errors: nvme
                .get("media_errors")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            num_err_log_entries: nvme
                .get("num_err_log_entries")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            power_on_hours: smart.power_on_hours,
            unsafe_shutdowns: nvme
                .get("unsafe_shutdowns")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            critical_warning: nvme
                .get("critical_warning")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
        };

        let critical_warning = nvme_data.critical_warning;
        let percentage_used = nvme_data.percentage_used;
        smart.nvme = Some(nvme_data);
        smart.health_percent = 100u32.saturating_sub(percentage_used);
        if critical_warning != 0 {
            smart.overall_status = "FAILED".to_string();
        }
    }

    recognized.then_some(smart)
}

// ── Subprocess helpers ────────────────────────────────────────────────────────

fn build_wmi_script(query: &str, namespace: &str) -> String {
    const TEMPLATE: &str = r#"
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch { }
$ns = '__NAMESPACE__'
$q = '__QUERY__'
$rows = $null
try {
    $rows = Get-CimInstance -Namespace $ns -Query $q -ErrorAction Stop
} catch {
    try {
        $rows = Get-WmiObject -Namespace $ns -Query $q -ErrorAction Stop
    } catch {
        $rows = @()
    }
}
$items = @($rows)
if ($items.Count -eq 0) {
    '[]'
} else {
    $items |
        Select-Object * -ExcludeProperty CimClass,CimInstanceProperties,CimSystemProperties,PSComputerName,__GENUS,__CLASS,__SUPERCLASS,__DYNASTY,__RELPATH,__PROPERTY_COUNT,__DERIVATION,__SERVER,__NAMESPACE,__PATH |
        ConvertTo-Json -Depth 4 -Compress
}
"#;

    TEMPLATE
        .replace("__NAMESPACE__", &namespace.replace('\'', "''"))
        .replace("__QUERY__", &query.replace('\'', "''"))
}

/// Run a command, draining stdout on a helper thread and polling for exit.
/// Returns `(success, stdout)` or `None` when spawn fails or the timeout hits.
fn run_command_bounded(program: &str, args: &[&str], timeout: Duration) -> Option<(bool, String)> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    };

    let output = reader.join().ok()?;
    Some((
        status.success(),
        String::from_utf8_lossy(&output).into_owned(),
    ))
}

// ── Value helpers ─────────────────────────────────────────────────────────────

fn json_str(row: &Value, key: &str) -> Option<String> {
    let value = row.get(key)?;
    let text = match value {
        Value::String(text) => text
            .trim_matches(|c: char| c.is_whitespace() || c == '\0')
            .to_string(),
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

fn json_f64(row: &Value, key: &str) -> Option<f64> {
    let number = match row.get(key)? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }?;

    number.is_finite().then_some(number)
}

fn json_u64(row: &Value, key: &str) -> Option<u64> {
    match row.get(key)? {
        Value::Number(number) => number.as_u64().or_else(|| {
            number
                .as_f64()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|value| value as u64)
        }),
        Value::String(text) => {
            let text = text.trim();
            text.parse::<u64>().ok().or_else(|| {
                text.parse::<f64>()
                    .ok()
                    .filter(|value| value.is_finite() && *value >= 0.0)
                    .map(|value| value as u64)
            })
        }
        Value::Bool(boolean) => Some(u64::from(*boolean)),
        _ => None,
    }
}

fn json_u32(row: &Value, key: &str) -> Option<u32> {
    json_u64(row, key).and_then(|value| u32::try_from(value).ok())
}

fn json_bool(row: &Value, key: &str) -> Option<bool> {
    match row.get(key)? {
        Value::Bool(boolean) => Some(*boolean),
        Value::Number(number) => Some(number.as_f64().is_some_and(|value| value != 0.0)),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn parse_f64_field(field: &str) -> Option<f64> {
    let field = field.trim();
    if field.is_empty()
        || field.eq_ignore_ascii_case("n/a")
        || field.eq_ignore_ascii_case("[n/a]")
        || field.eq_ignore_ascii_case("[not supported]")
    {
        return None;
    }

    field.parse::<f64>().ok().filter(|value| value.is_finite())
}

fn parse_u32_field(field: &str) -> Option<u32> {
    let field = field.trim();
    if field.is_empty() {
        return None;
    }

    field
        .parse::<u32>()
        .ok()
        .or_else(|| field.parse::<f64>().ok().map(|value| value as u32))
}

// ── Names and small mappings ──────────────────────────────────────────────────

fn memory_type_name(code: u64) -> String {
    match code {
        0 => "Unknown",
        1 => "Other",
        2 => "DRAM",
        20 => "DDR",
        21 => "DDR2",
        22 => "DDR2 FB-DIMM",
        24 => "DDR3",
        26 => "DDR4",
        27 => "LPDDR",
        28 => "LPDDR2",
        29 => "LPDDR3",
        30 => "LPDDR4",
        32 => "HBM",
        33 => "HBM2",
        34 => "DDR5",
        35 => "LPDDR5",
        36 => "HBM3",
        other => return format!("Type{other}"),
    }
    .to_string()
}

fn form_factor_name(code: u64) -> String {
    match code {
        7 => "SIMM",
        8 => "DIMM",
        11 => "RIMM",
        12 => "SO-DIMM",
        13 => "Micro-DIMM",
        _ => "",
    }
    .to_string()
}

fn derive_memory_channel(bank: &str, slot: &str) -> String {
    for text in [bank, slot] {
        let lower = text.to_ascii_lowercase();
        if let Some(position) = lower.find("channel") {
            let remainder = &text[position + "channel".len()..];
            let remainder = remainder.trim_start_matches([' ', '_', '-', ':']);
            if let Some(channel) = remainder.chars().next() {
                if channel.is_ascii_alphanumeric() {
                    return channel.to_ascii_uppercase().to_string();
                }
            }
        }
    }

    String::new()
}

fn gpu_vendor(name: &str) -> &'static str {
    let upper = name.to_ascii_uppercase();
    if upper.contains("NVIDIA") {
        "NVIDIA"
    } else if upper.contains("AMD") || upper.contains("RADEON") {
        "AMD"
    } else if upper.contains("INTEL") {
        "Intel"
    } else {
        ""
    }
}

fn is_solid_state(model: &str, media: &str, interface: &str) -> bool {
    let model = model.to_ascii_lowercase();
    let media = media.to_ascii_lowercase();
    let interface = interface.to_ascii_lowercase();

    media.contains("ssd")
        || interface.contains("nvme")
        || model.contains("ssd")
        || model.contains("nvme")
        || model.contains("solid state")
}

fn battery_status_name(status: u32) -> &'static str {
    match status {
        1 => "Discharging",
        2 => "AC+Battery",
        3 => "Full",
        4 => "Low",
        5 => "Critical",
        6 => "Charging",
        7 => "Charging+High",
        8 => "Charging+Low",
        9 => "Charging+Critical",
        _ => "Unknown",
    }
}

fn battery_chemistry_name(code: u32) -> &'static str {
    match code {
        1 => "Other",
        2 => "Unknown",
        3 => "Lead Acid",
        4 => "NiCd",
        5 => "NiMH",
        6 => "Li-ion",
        7 => "Zinc air",
        8 => "LiPo",
        _ => "",
    }
}

/// Convert a CIM datetime such as `20230815000000.000000-000` to `2023-08-15`.
fn format_cim_date(raw: &str) -> String {
    let digits: String = raw.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() >= 8 {
        format!("{}-{}-{}", &digits[0..4], &digits[4..6], &digits[6..8])
    } else {
        raw.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_snapshot() -> HardwareSnapshot {
        HardwareSnapshot {
            cpu: parse_win32_processor(
                r#"[{"Name":"Test CPU","NumberOfCores":2,"NumberOfLogicalProcessors":4,
                     "MaxClockSpeed":3000,"CurrentClockSpeed":2900,
                     "SocketDesignation":"LGA1700"}]"#,
            ),
            gpus: vec![GpuInfo {
                index: 0,
                model: "NVIDIA GeForce RTX 4090".to_string(),
                ..GpuInfo::default()
            }],
            ..HardwareSnapshot::default()
        }
    }

    #[test]
    fn wmi_rows_accept_single_object_and_array() {
        let array = r#"[{"Name":"A"},{"Name":"B"}]"#;
        let rows = parse_wmi_rows(array);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["Name"], "A");
        assert_eq!(rows[1]["Name"], "B");

        let single = r#"{"Name":"Only"}"#;
        let rows = parse_wmi_rows(single);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["Name"], "Only");
    }

    #[test]
    fn wmi_rows_handle_null_bom_empty_and_malformed() {
        assert!(parse_wmi_rows("null").is_empty());
        assert!(parse_wmi_rows("[]").is_empty());
        assert!(parse_wmi_rows("").is_empty());
        assert!(parse_wmi_rows("   \n\t ").is_empty());
        assert!(parse_wmi_rows("not json at all").is_empty());
        assert!(parse_wmi_rows(r#"{"Name":"#).is_empty());

        let rows = parse_wmi_rows("\u{feff}[{\"Name\":\"BOM\"}]");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["Name"], "BOM");
    }

    #[test]
    fn processor_maps_topology_and_cores() {
        let cpu = parse_win32_processor(
            r#"[{"Name":"AMD Ryzen 9 7950X 16-Core Processor",
                 "NumberOfCores":16,"NumberOfLogicalProcessors":32,
                 "MaxClockSpeed":4500,"CurrentClockSpeed":4200,
                 "SocketDesignation":"AM5"}]"#,
        );

        assert_eq!(cpu.brand, "AMD Ryzen 9 7950X 16-Core Processor");
        assert_eq!(cpu.model, cpu.brand);
        assert_eq!(cpu.socket, "AM5");
        assert_eq!(cpu.sockets, 1);
        assert_eq!(cpu.physical_cores, 16);
        assert_eq!(cpu.logical_cores, 32);
        assert!((cpu.base_clock_mhz - 4500.0).abs() < 0.1);
        assert_eq!(cpu.cores.len(), 32);
        assert_eq!(cpu.cores[0].index, 0);
        assert_eq!(cpu.cores[31].index, 31);
        assert!((cpu.cores[0].clock_mhz - 4200.0).abs() < 0.1);
    }

    #[test]
    fn processor_tolerates_missing_fields() {
        let cpu = parse_win32_processor(r#"[{"Name":"Mystery CPU"}]"#);
        assert_eq!(cpu.brand, "Mystery CPU");
        assert_eq!(cpu.physical_cores, 0);
        assert_eq!(cpu.logical_cores, 0);
        assert!(cpu.cores.is_empty());

        let empty = parse_win32_processor("[]");
        assert!(empty.brand.is_empty());
        assert_eq!(empty.sockets, 1);
    }

    #[test]
    fn memory_maps_types_form_factors_and_voltage() {
        let slots = parse_win32_memory(
            r#"[
                {"DeviceLocator":"DIMM_A1","BankLabel":"Controller0-ChannelA",
                 "Capacity":17179869184,"Speed":3200,"ConfiguredClockSpeed":2933,
                 "SMBIOSMemoryType":26,"Manufacturer":" G.Skill ",
                 "PartNumber":"F4-3200C16-16GVK ","SerialNumber":"  1234 ",
                 "FormFactor":8,"ConfiguredVoltage":1200},
                {"DeviceLocator":"DIMM_B1","BankLabel":"BANK 1","Capacity":17179869184,
                 "Speed":4800,"ConfiguredClockSpeed":4800,"SMBIOSMemoryType":0,
                 "MemoryType":34,"Manufacturer":"Samsung","FormFactor":12}
            ]"#,
        );

        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].slot, "DIMM_A1");
        assert_eq!(slots[0].bank, "Controller0-ChannelA");
        assert_eq!(slots[0].channel, "A");
        assert!((slots[0].size_gb - 16.0).abs() < 0.01);
        assert_eq!(slots[0].memory_type, "DDR4");
        assert_eq!(slots[0].form_factor, "DIMM");
        assert_eq!(slots[0].speed_mhz, 3200);
        assert_eq!(slots[0].configured_speed_mhz, 2933);
        assert_eq!(slots[0].manufacturer, "G.Skill");
        assert_eq!(slots[0].part_number, "F4-3200C16-16GVK");
        assert_eq!(slots[0].serial_number, "1234");
        assert!((slots[0].timing.voltage_v - 1.2).abs() < 0.0001);

        assert_eq!(slots[1].memory_type, "DDR5");
        assert_eq!(slots[1].form_factor, "SO-DIMM");
        assert_eq!(slots[1].channel, "");
    }

    #[test]
    fn memory_tolerates_missing_fields_and_skips_empty_capacity() {
        let slots = parse_win32_memory(
            r#"[{"Capacity":8589934592},{"DeviceLocator":"EMPTY","Capacity":0}]"#,
        );

        assert_eq!(slots.len(), 1);
        assert!((slots[0].size_gb - 8.0).abs() < 0.01);
        assert_eq!(slots[0].memory_type, "Unknown");
        assert_eq!(slots[0].form_factor, "");
        assert_eq!(slots[0].timing.voltage_v, 0.0);
    }

    #[test]
    fn nvidia_smi_parses_multiple_gpus() {
        let gpus = parse_nvidia_smi(
            "0, NVIDIA GeForce RTX 4090, 24564, 552.44, 45, 12, 23.50, 210, 10501\n\
             1, NVIDIA GeForce RTX 3060, 12288, 552.44, 38, 0, 8.00, 210, 7500\n",
        );

        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].index, 0);
        assert_eq!(gpus[0].vendor, "NVIDIA");
        assert_eq!(gpus[0].model, "NVIDIA GeForce RTX 4090");
        assert!((gpus[0].vram_gb - 24564.0 / 1024.0).abs() < 0.01);
        assert_eq!(gpus[0].driver_version, "552.44");
        assert!((gpus[0].temperature_c - 45.0).abs() < 0.01);
        assert!((gpus[0].utilization_percent - 12.0).abs() < 0.01);
        assert!((gpus[0].power_w - 23.5).abs() < 0.01);
        assert!((gpus[0].core_clock_mhz - 210.0).abs() < 0.01);
        assert!((gpus[0].memory_clock_mhz - 10501.0).abs() < 0.01);

        assert_eq!(gpus[1].index, 1);
        assert!((gpus[1].vram_gb - 12.0).abs() < 0.01);
        assert!((gpus[1].utilization_percent - 0.0).abs() < 0.01);
    }

    #[test]
    fn nvidia_smi_drops_unavailable_values_and_malformed_lines() {
        let gpus = parse_nvidia_smi(
            "0, Test GPU, [N/A], [N/A], [Not Supported], [N/A], [N/A], [N/A], [N/A]\n\
             not,a,valid,line\n",
        );

        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].model, "Test GPU");
        assert_eq!(gpus[0].vram_gb, 0.0);
        assert_eq!(gpus[0].temperature_c, 0.0);
        assert_eq!(gpus[0].power_w, 0.0);
        assert_eq!(gpus[0].core_clock_mhz, 0.0);
    }

    #[test]
    fn diskdrive_maps_identity_and_capacity() {
        let disks = parse_win32_diskdrive(
            r#"[{"DeviceID":"\\\\.\\PHYSICALDRIVE0",
                 "Model":"Samsung SSD 990 PRO 1TB","SerialNumber":"S1234567890",
                 "FirmwareRevision":"1B2QJXD7","Size":"1000204886016",
                 "MediaType":"Fixed hard disk media","InterfaceType":"SCSI"}]"#,
        );

        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].device, r"\\.\PHYSICALDRIVE0");
        assert_eq!(disks[0].model, "Samsung SSD 990 PRO 1TB");
        assert_eq!(disks[0].serial, "S1234567890");
        assert_eq!(disks[0].firmware, "1B2QJXD7");
        assert_eq!(disks[0].capacity_bytes, 1_000_204_886_016);
        assert!(!disks[0].rotational);
        assert!(disks[0].smart.is_none());
    }

    #[test]
    fn diskdrive_marks_hard_disks_rotational_and_tolerates_missing_fields() {
        let disks = parse_win32_diskdrive(
            r#"[{"DeviceID":"\\\\.\\PHYSICALDRIVE1","Model":"WDC WD40EFRX",
                 "Size":"4000787030016","MediaType":"Fixed hard disk media"},
                {"Model":"Mystery"}]"#,
        );

        assert_eq!(disks.len(), 2);
        assert_eq!(disks[0].capacity_bytes, 4_000_787_030_016);
        assert!(disks[0].rotational);
        assert_eq!(disks[1].device, "");
        assert_eq!(disks[1].capacity_bytes, 0);
    }

    #[test]
    fn lhm_sensors_map_cpu_gpu_fans_and_voltages() {
        let mut snapshot = test_snapshot();
        let sensors: Vec<Value> = serde_json::from_str(
            r#"[
                {"Name":"CPU Core #1","SensorType":"Temperature","Value":55.0,"Min":30.0,"Max":80.0,"Parent":"Test CPU"},
                {"Name":"CPU Package","SensorType":"Temperature","Value":62.0,"Min":30.0,"Max":90.0,"Parent":"Test CPU"},
                {"Name":"CPU Core #1","SensorType":"Clock","Value":4200.0,"Min":800.0,"Max":5000.0,"Parent":"Test CPU"},
                {"Name":"CPU Core","SensorType":"Voltage","Value":1.25,"Min":0.8,"Max":1.4,"Parent":"Test CPU"},
                {"Name":"CPU Package","SensorType":"Voltage","Value":1.3,"Min":0.8,"Max":1.4,"Parent":"Test CPU"},
                {"Name":"VIN","SensorType":"Voltage","Value":12.0,"Min":11.5,"Max":12.5,"Parent":"Nuvoton NCT6798D"},
                {"Name":"CPU Fan","SensorType":"Fan","Value":1200.0,"Min":0.0,"Max":2000.0,"Parent":"Nuvoton NCT6798D"},
                {"Name":"CPU Fan","SensorType":"Control","Value":55.0,"Min":0.0,"Max":100.0,"Parent":"Nuvoton NCT6798D"},
                {"Name":"CPU Core #1","SensorType":"Power","Value":20.0,"Min":0.0,"Max":50.0,"Parent":"Test CPU"},
                {"Name":"CPU Package","SensorType":"Power","Value":88.5,"Min":10.0,"Max":200.0,"Parent":"Test CPU"},
                {"Name":"CPU Cores","SensorType":"Power","Value":60.0,"Min":5.0,"Max":150.0,"Parent":"Test CPU"},
                {"Name":"CPU DRAM","SensorType":"Power","Value":12.0,"Min":0.0,"Max":30.0,"Parent":"Test CPU"},
                {"Name":"CPU Uncore","SensorType":"Power","Value":8.0,"Min":0.0,"Max":30.0,"Parent":"Test CPU"},
                {"Name":"GPU Core","SensorType":"Temperature","Value":65.0,"Min":30.0,"Max":90.0,"Parent":"NVIDIA GeForce RTX 4090"},
                {"Name":"GPU Hot Spot","SensorType":"Temperature","Value":78.0,"Min":30.0,"Max":100.0,"Parent":"NVIDIA GeForce RTX 4090"},
                {"Name":"GPU Memory Junction","SensorType":"Temperature","Value":70.0,"Min":30.0,"Max":100.0,"Parent":"NVIDIA GeForce RTX 4090"},
                {"Name":"GPU Memory","SensorType":"Temperature","Value":68.0,"Min":30.0,"Max":100.0,"Parent":"NVIDIA GeForce RTX 4090"},
                {"Name":"GPU Core","SensorType":"Clock","Value":2500.0,"Min":210.0,"Max":2800.0,"Parent":"NVIDIA GeForce RTX 4090"},
                {"Name":"GPU Memory","SensorType":"Clock","Value":10500.0,"Min":400.0,"Max":10501.0,"Parent":"NVIDIA GeForce RTX 4090"},
                {"Name":"GPU Package","SensorType":"Power","Value":250.0,"Min":20.0,"Max":450.0,"Parent":"NVIDIA GeForce RTX 4090"},
                {"Name":"GPU Core","SensorType":"Load","Value":88.0,"Min":0.0,"Max":100.0,"Parent":"NVIDIA GeForce RTX 4090"},
                {"Name":"GPU Memory Read","SensorType":"Throughput","Value":1024.0,"Min":0.0,"Max":4096.0,"Parent":"NVIDIA GeForce RTX 4090"},
                {"Name":"GPU Memory Used","SensorType":"SmallData","Value":4096.0,"Min":0.0,"Max":24576.0,"Parent":"NVIDIA GeForce RTX 4090"},
                {"Name":"VRM","SensorType":"Temperature","Value":50.0,"Min":20.0,"Max":100.0,"Parent":"Test Board"},
                {"Name":"PCH","SensorType":"Temperature","Value":45.0,"Min":20.0,"Max":100.0,"Parent":"Test Board"},
                {"Name":"Missing Value","SensorType":"Temperature","Value":null,"Parent":"Test Board"}
            ]"#,
        )
        .expect("fixture is valid JSON");

        apply_lhm_sensors(&mut snapshot, &sensors);

        assert!((snapshot.cpu.cores[0].temperature_c - 55.0).abs() < 0.01);
        assert!((snapshot.cpu.cores[0].clock_mhz - 4200.0).abs() < 0.01);
        assert!((snapshot.cpu.package_temp_c - 62.0).abs() < 0.01);
        assert!((snapshot.cpu.core_voltage_v - 1.3).abs() < 0.001);
        assert!((snapshot.cpu.package_power_w - 88.5).abs() < 0.01);
        assert!((snapshot.cpu.cores[0].power_w - 20.0).abs() < 0.01);
        assert!((snapshot.cpu.core_power_w - 60.0).abs() < 0.01);
        assert!((snapshot.cpu.dram_power_w - 12.0).abs() < 0.01);
        assert!((snapshot.cpu.uncore_power_w - 8.0).abs() < 0.01);

        assert_eq!(snapshot.fans.len(), 1);
        assert_eq!(snapshot.fans[0].name, "CPU Fan");
        assert_eq!(snapshot.fans[0].rpm, 1200);
        assert!((snapshot.fans[0].percent - 55.0).abs() < 0.01);
        assert!(snapshot.fans[0].controllable);

        assert_eq!(snapshot.voltages.len(), 1);
        let voltage = &snapshot.voltages[0];
        assert_eq!(voltage.name, "VIN");
        assert!((voltage.voltage_v - 12.0).abs() < 0.001);
        assert!((voltage.min_v - 11.5).abs() < 0.001);
        assert!((voltage.max_v - 12.5).abs() < 0.001);

        let gpu = &snapshot.gpus[0];
        assert!((gpu.temperature_c - 65.0).abs() < 0.01);
        assert!((gpu.hotspot_temp_c - 78.0).abs() < 0.01);
        assert!((gpu.vram_temp_c - 68.0).abs() < 0.01);
        assert!((gpu.core_clock_mhz - 2500.0).abs() < 0.01);
        assert!((gpu.memory_clock_mhz - 10500.0).abs() < 0.01);
        assert!((gpu.power_w - 250.0).abs() < 0.01);
        assert!((gpu.utilization_percent - 88.0).abs() < 0.01);
        assert!((gpu.memory_bandwidth_gbs - 1.0).abs() < 0.001);
        assert!((gpu.vram_used_gb - 4.0).abs() < 0.001);

        assert!((snapshot.motherboard.vrm_temp_c - 50.0).abs() < 0.01);
        assert!((snapshot.motherboard.pch_temp_c - 45.0).abs() < 0.01);
    }

    #[test]
    fn lhm_sensors_ignore_non_finite_and_empty_rows() {
        let mut snapshot = test_snapshot();
        let sensors: Vec<Value> = serde_json::from_str(
            r#"[
                {},
                {"Name":"CPU Package","SensorType":"Temperature","Value":"not a number"},
                {"Name":"CPU Package","SensorType":"Temperature","Value":null},
                {"Name":"CPU Package","SensorType":"Temperature","Value":70.5}
            ]"#,
        )
        .expect("fixture is valid JSON");

        apply_lhm_sensors(&mut snapshot, &sensors);

        assert!((snapshot.cpu.package_temp_c - 70.5).abs() < 0.01);
        assert!(snapshot.voltages.is_empty());
        assert!(snapshot.fans.is_empty());
    }

    #[test]
    fn smart_json_maps_ata_and_nvme_health() {
        let ata: Value = serde_json::from_str(
            r#"{
                "smart_status":{"passed":true},
                "power_on_time":{"hours":1234},
                "temperature":{"current":41},
                "ata_smart_attributes":{"table":[
                    {"id":5,"name":"Reallocated_Sector_Ct","value":100,"worst":100,
                     "thresh":10,"raw":{"value":2},"flags":{"value":32,"prefailure":true}},
                    {"id":197,"name":"Current_Pending_Sector","value":100,"worst":100,
                     "thresh":0,"raw":{"value":1},"flags":{"value":0,"prefailure":false}}
                ]}
            }"#,
        )
        .expect("valid ATA fixture");

        let smart = parse_smart_json(&ata).expect("ATA health");
        assert_eq!(smart.overall_status, "PASSED");
        assert_eq!(smart.power_on_hours, 1234);
        assert!((smart.temperature_c - 41.0).abs() < 0.01);
        assert_eq!(smart.reallocated_sectors, 2);
        assert_eq!(smart.pending_sectors, 1);
        assert_eq!(smart.attributes.len(), 2);
        assert_eq!(smart.health_percent, 94);
        assert!(smart.nvme.is_none());

        let nvme: Value = serde_json::from_str(
            r#"{
                "smart_status":{"passed":true},
                "nvme_smart_health_information_log":{
                    "available_spare":100,"available_spare_threshold":10,
                    "percentage_used":3,"data_units_written":1000,"data_units_read":2000,
                    "host_write_commands":3000,"host_read_commands":4000,
                    "media_errors":0,"num_err_log_entries":1,
                    "power_on_hours":500,"unsafe_shutdowns":2,"critical_warning":0
                }
            }"#,
        )
        .expect("valid NVMe fixture");

        let smart = parse_smart_json(&nvme).expect("NVMe health");
        assert_eq!(smart.health_percent, 97);
        assert_eq!(smart.power_on_hours, 500);
        let nvme = smart.nvme.expect("NVMe payload");
        assert_eq!(nvme.percentage_used, 3);
        assert_eq!(nvme.power_on_hours, 500);
        assert_eq!(nvme.critical_warning, 0);
    }
}
