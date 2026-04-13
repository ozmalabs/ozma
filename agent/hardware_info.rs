// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Ozma Hardware Info — HWiNFO64-parity hardware enumeration and sensor monitoring.
//!
//! Runs inside the target machine (via ozma agent) and provides:
//!
//! 1. Full hardware tree: CPU topology + cache + instructions, memory SPD + XMP,
//!    GPU clocks/power/fan/bandwidth, storage SMART + NVMe wear, motherboard
//!    VRM/PCH, PCIe topology, battery health, all voltage rails, all fan RPMs.
//!
//! 2. Real-time sensor polling at 1 Hz (configurable): per-core temperatures,
//!    per-core clocks, RAPL package/core/uncore power, GPU hot-spot, VRAM temp,
//!    memory bandwidth, per-disk read/write rates.
//!
//! 3. Full SMART attribute table (ID, name, value, worst, raw, threshold).
//!
//! 4. Report generation: JSON, text, CSV, HTML — equivalent to HWiNFO64 summary.
//!
//! 5. Prometheus metrics integration: extend prometheus_metrics.py with deep
//!    hardware sensors as additional ozma_node_* gauge metrics.

use std::time::{SystemTime, UNIX_EPOCH};
use std::process::Command;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use serde::{Deserialize, Serialize};
use anyhow::{Result, Context, anyhow};
use sysinfo::{System, CpuExt, SystemExt};

// Data structures for hardware information

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheInfo {
    pub level: u32,
    pub size_kb: u32,
    pub cache_type: String,
    pub ways: u32,
    pub sets: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuCoreInfo {
    pub index: u32,
    pub temperature_c: f32,
    pub clock_mhz: f32,
    pub voltage_v: f32,
    pub usage_percent: f32,
    pub power_w: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuPackageInfo {
    pub brand: String,
    pub model: String,
    pub socket: String,
    pub sockets: u32,
    pub physical_cores: u32,
    pub logical_cores: u32,
    pub base_clock_mhz: f32,
    pub boost_clock_mhz: f32,
    pub tdp_w: f32,
    pub microcode: String,
    // Sensors
    pub package_temp_c: f32,
    pub package_power_w: f32,
    pub core_power_w: f32,
    pub uncore_power_w: f32,
    pub dram_power_w: f32,
    pub core_voltage_v: f32,
    // Instruction sets
    pub instructions: Vec<String>,
    // Cache hierarchy
    pub caches: Vec<CacheInfo>,
    // Per-core data
    pub cores: Vec<CpuCoreInfo>,
}

impl Default for CpuPackageInfo {
    fn default() -> Self {
        Self {
            brand: String::new(),
            model: String::new(),
            socket: String::new(),
            sockets: 1,
            physical_cores: 0,
            logical_cores: 0,
            base_clock_mhz: 0.0,
            boost_clock_mhz: 0.0,
            tdp_w: 0.0,
            microcode: String::new(),
            package_temp_c: 0.0,
            package_power_w: 0.0,
            core_power_w: 0.0,
            uncore_power_w: 0.0,
            dram_power_w: 0.0,
            core_voltage_v: 0.0,
            instructions: Vec::new(),
            caches: Vec::new(),
            cores: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryTiming {
    pub cl: u32,
    pub trcd: u32,
    pub trp: u32,
    pub tras: u32,
    pub trc: u32,
    pub voltage_v: f32,
}

impl Default for MemoryTiming {
    fn default() -> Self {
        Self {
            cl: 0,
            trcd: 0,
            trp: 0,
            tras: 0,
            trc: 0,
            voltage_v: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryXmpProfile {
    pub number: u32,
    pub speed_mhz: u32,
    pub timing: MemoryTiming,
    pub voltage_v: f32,
}

impl Default for MemoryXmpProfile {
    fn default() -> Self {
        Self {
            number: 0,
            speed_mhz: 0,
            timing: MemoryTiming::default(),
            voltage_v: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySlotInfo {
    pub slot: String,
    pub size_gb: f32,
    pub memory_type: String,
    pub speed_mhz: u32,
    pub configured_speed_mhz: u32,
    pub manufacturer: String,
    pub part_number: String,
    pub serial_number: String,
    pub form_factor: String,
    pub rank: String,
    pub bank: String,
    pub channel: String,
    pub timing: MemoryTiming,
    pub xmp_profiles: Vec<MemoryXmpProfile>,
    // Live sensors (if available)
    pub temperature_c: f32,
    pub bandwidth_read_gbs: f32,
    pub bandwidth_write_gbs: f32,
}

impl Default for MemorySlotInfo {
    fn default() -> Self {
        Self {
            slot: String::new(),
            size_gb: 0.0,
            memory_type: String::new(),
            speed_mhz: 0,
            configured_speed_mhz: 0,
            manufacturer: String::new(),
            part_number: String::new(),
            serial_number: String::new(),
            form_factor: String::new(),
            rank: String::new(),
            bank: String::new(),
            channel: String::new(),
            timing: MemoryTiming::default(),
            xmp_profiles: Vec::new(),
            temperature_c: 0.0,
            bandwidth_read_gbs: 0.0,
            bandwidth_write_gbs: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuInfo {
    pub index: u32,
    pub vendor: String,
    pub model: String,
    pub vram_gb: f32,
    pub pcie_slot: i32,
    pub pcie_width: u32,
    pub pcie_gen: u32,
    pub driver_version: String,
    pub bios_version: String,
    pub cuda_cores: u32,
    pub shader_processors: u32,
    pub rops: u32,
    pub tmus: u32,
    // Live sensors
    pub temperature_c: f32,
    pub hotspot_temp_c: f32,
    pub vram_temp_c: f32,
    pub core_clock_mhz: f32,
    pub memory_clock_mhz: f32,
    pub shader_clock_mhz: f32,
    pub power_w: f32,
    pub power_limit_w: f32,
    pub fan_rpm: u32,
    pub fan_percent: f32,
    pub utilization_percent: f32,
    pub vram_used_gb: f32,
    pub memory_bandwidth_gbs: f32,
    pub nvenc_usage_percent: f32,
    pub nvdec_usage_percent: f32,
    pub pcie_bandwidth_mbs: f32,
}

impl Default for GpuInfo {
    fn default() -> Self {
        Self {
            index: 0,
            vendor: String::new(),
            model: String::new(),
            vram_gb: 0.0,
            pcie_slot: -1,
            pcie_width: 0,
            pcie_gen: 0,
            driver_version: String::new(),
            bios_version: String::new(),
            cuda_cores: 0,
            shader_processors: 0,
            rops: 0,
            tmus: 0,
            temperature_c: 0.0,
            hotspot_temp_c: 0.0,
            vram_temp_c: 0.0,
            core_clock_mhz: 0.0,
            memory_clock_mhz: 0.0,
            shader_clock_mhz: 0.0,
            power_w: 0.0,
            power_limit_w: 0.0,
            fan_rpm: 0,
            fan_percent: 0.0,
            utilization_percent: 0.0,
            vram_used_gb: 0.0,
            memory_bandwidth_gbs: 0.0,
            nvenc_usage_percent: 0.0,
            nvdec_usage_percent: 0.0,
            pcie_bandwidth_mbs: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartAttribute {
    pub id: u32,
    pub name: String,
    pub value: u32,
    pub worst: u32,
    pub raw: u64,
    pub threshold: u32,
    pub flags: u32,
    // Pre-fail flag means this attribute predicts failure if below threshold
    pub pre_fail: bool,
    // Whether this attribute is currently failing
    pub failing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NvmeData {
    pub temperature_c: f32,
    pub available_spare_percent: u32,
    pub available_spare_threshold_percent: u32,
    pub percentage_used: u32,
    pub data_units_written: u64,
    pub data_units_read: u64,
    pub host_write_commands: u64,
    pub host_read_commands: u64,
    pub media_errors: u64,
    pub num_err_log_entries: u64,
    pub power_on_hours: u64,
    pub unsafe_shutdowns: u64,
    pub critical_warning: u32,
}

impl Default for NvmeData {
    fn default() -> Self {
        Self {
            temperature_c: 0.0,
            available_spare_percent: 0,
            available_spare_threshold_percent: 0,
            percentage_used: 0,
            data_units_written: 0,
            data_units_read: 0,
            host_write_commands: 0,
            host_read_commands: 0,
            media_errors: 0,
            num_err_log_entries: 0,
            power_on_hours: 0,
            unsafe_shutdowns: 0,
            critical_warning: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartData {
    pub health_percent: u32,
    pub overall_status: String,
    pub power_on_hours: u64,
    pub start_stop_count: u64,
    pub reallocated_sectors: u64,
    pub pending_sectors: u64,
    pub uncorrectable_errors: u64,
    pub temperature_c: f32,
    pub attributes: Vec<SmartAttribute>,
    pub self_test_result: String,
    // NVMe-specific (populated for NVMe devices)
    pub nvme: Option<NvmeData>,
}

impl Default for SmartData {
    fn default() -> Self {
        Self {
            health_percent: 100,
            overall_status: "PASSED".to_string(),
            power_on_hours: 0,
            start_stop_count: 0,
            reallocated_sectors: 0,
            pending_sectors: 0,
            uncorrectable_errors: 0,
            temperature_c: 0.0,
            attributes: Vec::new(),
            self_test_result: String::new(),
            nvme: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageInfo {
    pub device: String,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub interface: String,
    pub form_factor: String,
    pub capacity_bytes: u64,
    pub rotational: bool,
    pub rpm: u32,
    // Live sensors
    pub temperature_c: f32,
    pub read_rate_mbs: f32,
    pub write_rate_mbs: f32,
    // Full SMART
    pub smart: Option<SmartData>,
}

impl Default for StorageInfo {
    fn default() -> Self {
        Self {
            device: String::new(),
            model: String::new(),
            serial: String::new(),
            firmware: String::new(),
            interface: String::new(),
            form_factor: String::new(),
            capacity_bytes: 0,
            rotational: false,
            rpm: 0,
            temperature_c: 0.0,
            read_rate_mbs: 0.0,
            write_rate_mbs: 0.0,
            smart: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MotherboardInfo {
    pub manufacturer: String,
    pub model: String,
    pub version: String,
    pub bios_vendor: String,
    pub bios_version: String,
    pub bios_date: String,
    pub chipset: String,
    pub form_factor: String,
    // Live sensors
    pub vrm_temp_c: f32,
    pub pch_temp_c: f32,
    pub ambient_temp_c: f32,
}

impl Default for MotherboardInfo {
    fn default() -> Self {
        Self {
            manufacturer: String::new(),
            model: String::new(),
            version: String::new(),
            bios_vendor: String::new(),
            bios_version: String::new(),
            bios_date: String::new(),
            chipset: String::new(),
            form_factor: String::new(),
            vrm_temp_c: 0.0,
            pch_temp_c: 0.0,
            ambient_temp_c: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanReading {
    pub name: String,
    pub rpm: u32,
    pub percent: f32,
    pub target_rpm: u32,
    pub controllable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoltageReading {
    pub name: String,
    pub voltage_v: f32,
    pub min_v: f32,
    pub max_v: f32,
    pub nominal_v: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatteryInfo {
    pub name: String,
    pub status: String,
    pub capacity_design_mwh: u32,
    pub capacity_full_mwh: u32,
    pub capacity_now_mwh: u32,
    pub charge_rate_mw: u32,
    pub voltage_mv: u32,
    pub temperature_c: f32,
    pub cycle_count: u32,
    pub technology: String,
}

impl BatteryInfo {
    pub fn health_percent(&self) -> u32 {
        if self.capacity_design_mwh > 0 {
            (self.capacity_full_mwh * 100 / self.capacity_design_mwh).min(100)
        } else {
            0
        }
    }

    pub fn charge_percent(&self) -> f32 {
        if self.capacity_full_mwh > 0 {
            (self.capacity_now_mwh * 100) as f32 / self.capacity_full_mwh as f32
        } else {
            0.0
        }
    }
}

impl Default for BatteryInfo {
    fn default() -> Self {
        Self {
            name: String::new(),
            status: String::new(),
            capacity_design_mwh: 0,
            capacity_full_mwh: 0,
            capacity_now_mwh: 0,
            charge_rate_mw: 0,
            voltage_mv: 0,
            temperature_c: 0.0,
            cycle_count: 0,
            technology: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PcieDevice {
    pub slot: String,
    pub class_name: String,
    pub vendor: String,
    pub device: String,
    pub subsystem: String,
    pub driver: String,
    pub width: u32,
    pub gen: u32,
}

impl Default for PcieDevice {
    fn default() -> Self {
        Self {
            slot: String::new(),
            class_name: String::new(),
            vendor: String::new(),
            device: String::new(),
            subsystem: String::new(),
            driver: String::new(),
            width: 0,
            gen: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInfo {
    pub name: String,
    pub mac: String,
    pub speed_mbps: u32,
    pub duplex: String,
    pub driver: String,
    pub link_up: bool,
    // Live counters
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_rate_mbs: f32,
    pub tx_rate_mbs: f32,
    pub rx_errors: u64,
    pub tx_errors: u64,
}

impl Default for NetworkInfo {
    fn default() -> Self {
        Self {
            name: String::new(),
            mac: String::new(),
            speed_mbps: 0,
            duplex: String::new(),
            driver: String::new(),
            link_up: false,
            rx_bytes: 0,
            tx_bytes: 0,
            rx_rate_mbs: 0.0,
            tx_rate_mbs: 0.0,
            rx_errors: 0,
            tx_errors: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareSnapshot {
    /// Full point-in-time hardware inventory + sensor readings.
    pub timestamp: f64,
    pub hostname: String,
    pub os: String,
    pub os_version: String,
    pub cpu: CpuPackageInfo,
    pub memory_slots: Vec<MemorySlotInfo>,
    pub gpus: Vec<GpuInfo>,
    pub storage: Vec<StorageInfo>,
    pub motherboard: MotherboardInfo,
    pub fans: Vec<FanReading>,
    pub voltages: Vec<VoltageReading>,
    pub batteries: Vec<BatteryInfo>,
    pub pcie_devices: Vec<PcieDevice>,
    pub network: Vec<NetworkInfo>,
    pub total_memory_gb: f32,
}

impl Default for HardwareSnapshot {
    fn default() -> Self {
        Self {
            timestamp: 0.0,
            hostname: String::new(),
            os: String::new(),
            os_version: String::new(),
            cpu: CpuPackageInfo::default(),
            memory_slots: Vec::new(),
            gpus: Vec::new(),
            storage: Vec::new(),
            motherboard: MotherboardInfo::default(),
            fans: Vec::new(),
            voltages: Vec::new(),
            batteries: Vec::new(),
            pcie_devices: Vec::new(),
            network: Vec::new(),
            total_memory_gb: 0.0,
        }
    }
}

// Hardware collector trait
pub trait HardwareCollector {
    fn collect(&self) -> Result<HardwareSnapshot>;
}

// Linux implementation
pub struct LinuxHardwareCollector {
    prev_net: HashMap<String, (u64, u64, f64)>,
    prev_disk: HashMap<String, (u64, u64, f64)>,
}

impl LinuxHardwareCollector {
    pub fn new() -> Self {
        Self {
            prev_net: HashMap::new(),
            prev_disk: HashMap::new(),
        }
    }

    fn get_timestamp(&self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
    }
    
    fn read_file_to_string(&self, path: &str) -> Result<String> {
        fs::read_to_string(path)
            .with_context(|| format!("Failed to read file: {}", path))
    }
    
    fn read_file_to_int(&self, path: &str) -> Result<i64> {
        let content = self.read_file_to_string(path)?;
        content.trim().parse::<i64>()
            .with_context(|| format!("Failed to parse integer from file: {}", path))
    }
}

impl HardwareCollector for LinuxHardwareCollector {
    fn collect(&self) -> Result<HardwareSnapshot> {
        let mut snap = HardwareSnapshot::default();
        snap.timestamp = self.get_timestamp();
        
        // Collect basic system info
        snap.hostname = get_hostname();
        snap.os = "Linux".to_string();
        snap.os_version = get_os_version();
        
        // Collect hardware components
        snap.cpu = self.collect_cpu()?;
        snap.memory_slots = self.collect_memory()?;
        snap.total_memory_gb = snap.memory_slots.iter().map(|s| s.size_gb).sum();
        snap.gpus = self.collect_gpus()?;
        snap.storage = self.collect_storage()?;
        snap.motherboard = self.collect_motherboard()?;
        
        let (fans, voltages) = self.collect_hwmon_fans_voltages()?;
        snap.fans = fans;
        snap.voltages = voltages;
        
        snap.batteries = self.collect_batteries()?;
        snap.pcie_devices = self.collect_pcie()?;
        snap.network = self.collect_network()?;
        
        Ok(snap)
    }
}

impl LinuxHardwareCollector {
    fn collect_cpu(&self) -> Result<CpuPackageInfo> {
        let mut cpu = CpuPackageInfo::default();
        
        // Get basic CPU info from /proc/cpuinfo
        let cpuinfo = self.read_file_to_string("/proc/cpuinfo")?;
        for line in cpuinfo.lines() {
            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() != 2 {
                continue;
            }
            
            let key = parts[0].trim();
            let value = parts[1].trim();
            
            match key {
                "model name" => {
                    if cpu.brand.is_empty() {
                        cpu.brand = value.to_string();
                        cpu.model = value.to_string();
                    }
                },
                "vendor_id" => {
                    if cpu.socket.is_empty() {
                        cpu.socket = if value.contains("AMD") {
                            "AM5".to_string()
                        } else if value.contains("Intel") {
                            "LGA".to_string()
                        } else {
                            value.to_string()
                        };
                    }
                },
                "cpu MHz" => {
                    if cpu.base_clock_mhz == 0.0 {
                        if let Ok(freq) = value.parse::<f32>() {
                            cpu.base_clock_mhz = freq;
                        }
                    }
                },
                "flags" => {
                    if cpu.instructions.is_empty() {
                        let known_flags = [
                            "avx", "avx2", "avx512f", "avx512bw", "avx512cd",
                            "avx512dq", "avx512vl", "avx512vnni", "avx512bf16",
                            "amx_bf16", "amx_tile", "sse4_2", "sse4_1", "sse4a",
                            "aes", "pclmulqdq", "sha_ni", "vaes", "vpclmulqdq",
                            "fma", "f16c", "bmi1", "bmi2", "adx", "rdseed",
                            "rdrand", "clmul", "cx16", "movbe", "popcnt",
                            "tsc_deadline_timer", "xsave", "xsavec",
                            "hypervisor", "ept", "vnmi", "x2apic", "lm"
                        ];
                        
                        for flag in value.split_whitespace() {
                            if known_flags.contains(&flag) {
                                cpu.instructions.push(flag.to_uppercase());
                            }
                        }
                        cpu.instructions.sort();
                    }
                },
                _ => {}
            }
        }
        
        // Get socket count
        let physical_ids: Vec<&str> = cpuinfo
            .lines()
            .filter_map(|line| {
                if line.starts_with("physical id") {
                    line.split(':').nth(1).map(|s| s.trim())
                } else {
                    None
                }
            })
            .collect();
        cpu.sockets = physical_ids.iter().collect::<std::collections::HashSet<_>>().len() as u32;
        if cpu.sockets == 0 {
            cpu.sockets = 1;
        }
        
        // Get core counts
        let cores_list: Vec<&str> = cpuinfo
            .lines()
            .filter_map(|line| {
                if line.starts_with("cpu cores") {
                    line.split(':').nth(1).map(|s| s.trim())
                } else {
                    None
                }
            })
            .collect();
        
        if let Some(cores_str) = cores_list.first() {
            if let Ok(cores_per_socket) = cores_str.parse::<u32>() {
                cpu.physical_cores = cores_per_socket * cpu.sockets;
            }
        }
        
        // Use sysinfo for logical cores
        let mut sys = System::new_all();
        sys.refresh_cpu();
        cpu.logical_cores = sys.cpus().len() as u32;
        if cpu.logical_cores == 0 {
            cpu.logical_cores = 1;
        }
        
        // Get microcode from /proc/cpuinfo
        for line in cpuinfo.lines() {
            if line.starts_with("microcode") {
                let parts: Vec<&str> = line.split(':').collect();
                if parts.len() == 2 {
                    cpu.microcode = parts[1].trim().to_string();
                    break;
                }
            }
        }
        
        // Collect per-core data
        cpu.cores = self.collect_cpu_cores(cpu.logical_cores)?;
        
        // Package temperature
        let pkg_temp = self.read_hwmon_package_temp()?;
        if pkg_temp > 0.0 {
            cpu.package_temp_c = pkg_temp;
        } else if !cpu.cores.is_empty() {
            cpu.package_temp_c = cpu.cores.iter()
                .map(|c| c.temperature_c)
                .fold(0.0, f32::max);
        }
        
        // RAPL power
        let (pkg_power, core_power, uncore_power, dram_power) = self.read_rapl_power()?;
        cpu.package_power_w = pkg_power;
        cpu.core_power_w = core_power;
        cpu.uncore_power_w = uncore_power;
        cpu.dram_power_w = dram_power;
        
        // VCore voltage
        cpu.core_voltage_v = self.read_vcore()?;
        
        Ok(cpu)
    }
    
    fn collect_cpu_cores(&self, num_cores: u32) -> Result<Vec<CpuCoreInfo>> {
        let mut cores = Vec::new();
        let hwmon_temps = self.read_hwmon_core_temps()?;
        
        for i in 0..num_cores {
            let mut core = CpuCoreInfo {
                index: i,
                ..Default::default()
            };
            
            // Clock frequency
            let freq_path = format!("/sys/devices/system/cpu/cpu{}/cpufreq/scaling_cur_freq", i);
            if Path::new(&freq_path).exists() {
                if let Ok(freq_str) = self.read_file_to_string(&freq_path) {
                    if let Ok(freq) = freq_str.trim().parse::<i64>() {
                        core.clock_mhz = (freq as f32) / 1000.0;
                    }
                }
            }
            
            // Temperature
            if (i as usize) < hwmon_temps.len() {
                core.temperature_c = hwmon_temps[i as usize];
            }
            
            cores.push(core);
        }
        
        Ok(cores)
    }
    
    fn read_hwmon_core_temps(&self) -> Result<Vec<f32>> {
        let mut temps = HashMap::new();
        let hwmon_base = "/sys/class/hwmon";
        
        if !Path::new(hwmon_base).exists() {
            return Ok(Vec::new());
        }
        
        let paths = fs::read_dir(hwmon_base)?;
        for entry in paths {
            let entry = entry?;
            let path = entry.path();
            
            if !path.is_dir() {
                continue;
            }
            
            let name_path = path.join("name");
            if !name_path.exists() {
                continue;
            }
            
            let name = self.read_file_to_string(name_path.to_str().unwrap_or(""))?
                .trim()
                .to_string();
            
            if !["coretemp", "k10temp", "zenpower", "nct6775", 
                 "nct6779", "w83795g"].contains(&name.as_str()) {
                continue;
            }
            
            let temp_inputs = path.join("temp*_input");
            // In real implementation, we'd glob this pattern
            // For now, we'll check a few common ones
            for i in 1..=16 {
                let inp_path = path.join(format!("temp{}_input", i));
                if !inp_path.exists() {
                    continue;
                }
                
                let label_path = path.join(format!("temp{}_label", i));
                let label = if label_path.exists() {
                    self.read_file_to_string(label_path.to_str().unwrap_or(""))?
                        .trim()
                        .to_string()
                } else {
                    String::new()
                };
                
                if label.contains("Core") {
                    if let Ok(temp_str) = self.read_file_to_string(inp_path.to_str().unwrap_or("")) {
                        if let Ok(temp) = temp_str.trim().parse::<f32>() {
                            let core_idx = label.chars()
                                .filter(|c| c.is_digit(10))
                                .collect::<String>()
                                .parse::<usize>()
                                .unwrap_or(0);
                            temps.insert(core_idx, temp / 1000.0);
                        }
                    }
                }
            }
        }
        
        if temps.is_empty() {
            return Ok(Vec::new());
        }
        
        let max_idx = *temps.keys().max().unwrap_or(&0);
        let mut result = Vec::new();
        for i in 0..=max_idx {
            result.push(*temps.get(&i).unwrap_or(&0.0));
        }
        
        Ok(result)
    }
    
    fn read_hwmon_package_temp(&self) -> Result<f32> {
        let hwmon_base = "/sys/class/hwmon";
        
        if !Path::new(hwmon_base).exists() {
            return Ok(0.0);
        }
        
        let paths = fs::read_dir(hwmon_base)?;
        for entry in paths {
            let entry = entry?;
            let path = entry.path();
            
            if !path.is_dir() {
                continue;
            }
            
            let name_path = path.join("name");
            if !name_path.exists() {
                continue;
            }
            
            let name = self.read_file_to_string(name_path.to_str().unwrap_or(""))?
                .trim()
                .to_string();
            
            if !["coretemp", "k10temp", "zenpower"].contains(&name.as_str()) {
                continue;
            }
            
            let temp_inputs = path.join("temp*_input");
            // Check common temp inputs
            for i in 1..=8 {
                let inp_path = path.join(format!("temp{}_input", i));
                if !inp_path.exists() {
                    continue;
                }
                
                let label_path = path.join(format!("temp{}_label", i));
                let label = if label_path.exists() {
                    self.read_file_to_string(label_path.to_str().unwrap_or(""))?
                        .trim()
                        .to_string()
                } else {
                    String::new()
                };
                
                if label.contains("Package") || label.contains("Tdie") || label.contains("Tccd") {
                    if let Ok(temp_str) = self.read_file_to_string(inp_path.to_str().unwrap_or("")) {
                        if let Ok(temp) = temp_str.trim().parse::<f32>() {
                            return Ok(temp / 1000.0);
                        }
                    }
                }
            }
        }
        
        Ok(0.0)
    }
    
    fn read_rapl_power(&self) -> Result<(f32, f32, f32, f32)> {
        let rapl_base = "/sys/class/powercap";
        
        if !Path::new(rapl_base).exists() {
            return Ok((0.0, 0.0, 0.0, 0.0));
        }
        
        let mut pkg = 0.0;
        let mut core = 0.0;
        let mut uncore = 0.0;
        let mut dram = 0.0;
        
        let paths = fs::read_dir(rapl_base)?;
        for entry in paths {
            let entry = entry?;
            let path = entry.path();
            
            if !path.is_dir() {
                continue;
            }
            
            let name_path = path.join("name");
            let energy_path = path.join("energy_uj");
            
            if !name_path.exists() || !energy_path.exists() {
                continue;
            }
            
            let name = self.read_file_to_string(name_path.to_str().unwrap_or(""))?
                .trim()
                .to_string();
            
            // Sample energy over 100ms for instantaneous power
            if let Ok(e1_str) = self.read_file_to_string(energy_path.to_str().unwrap_or("")) {
                if let Ok(e1) = e1_str.trim().parse::<f64>() {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    
                    if let Ok(e2_str) = self.read_file_to_string(energy_path.to_str().unwrap_or("")) {
                        if let Ok(e2) = e2_str.trim().parse::<f64>() {
                            let power = ((e2 - e1) / 1e5) as f32; // uj/0.1s → watts
                            
                            if name.contains("package") && !name.contains("core") {
                                pkg += power;
                            } else if name.contains("core") {
                                core += power;
                            } else if name.contains("uncore") {
                                uncore += power;
                            } else if name.contains("dram") {
                                dram += power;
                            }
                        }
                    }
                }
            }
        }
        
        Ok((pkg, core, uncore, dram))
    }
    
    fn read_vcore(&self) -> Result<f32> {
        let hwmon_base = "/sys/class/hwmon";
        
        if !Path::new(hwmon_base).exists() {
            return Ok(0.0);
        }
        
        let paths = fs::read_dir(hwmon_base)?;
        for entry in paths {
            let entry = entry?;
            let path = entry.path();
            
            if !path.is_dir() {
                continue;
            }
            
            let name_path = path.join("name");
            if !name_path.exists() {
                continue;
            }
            
            let name = self.read_file_to_string(name_path.to_str().unwrap_or(""))?
                .trim()
                .to_string();
            
            let valid_names = [
                "nct6775", "nct6779", "nct6776", "nct6791",
                "nct6792", "nct6793", "nct6795", "nct6796",
                "nct6797", "nct6798", "it8720f", "it8728f",
                "it8771e", "w83627ehf", "asus_ec"
            ];
            
            if !valid_names.contains(&name.as_str()) {
                continue;
            }
            
            // Check voltage inputs
            for i in 0..=16 {
                let inp_path = path.join(format!("in{}_input", i));
                if !inp_path.exists() {
                    continue;
                }
                
                let label_path = path.join(format!("in{}_label", i));
                let label = if label_path.exists() {
                    self.read_file_to_string(label_path.to_str().unwrap_or(""))?
                        .trim()
                        .to_lowercase()
                } else {
                    String::new()
                };
                
                if label.contains("vcore") || label.contains("cpu") {
                    if let Ok(volt_str) = self.read_file_to_string(inp_path.to_str().unwrap_or("")) {
                        if let Ok(volt) = volt_str.trim().parse::<f32>() {
                            return Ok(volt / 1000.0);
                        }
                    }
                }
            }
        }
        
        Ok(0.0)
    }
    
    fn collect_memory(&self) -> Result<Vec<MemorySlotInfo>> {
        let mut slots = Vec::new();
        
        // Try to run dmidecode
        let output = Command::new("dmidecode")
            .args(&["-t", "memory"])
            .output();
            
        if let Ok(output) = output {
            if output.status.success() {
                let dmi_output = String::from_utf8_lossy(&output.stdout);
                
                let mut current_slot: Option<MemorySlotInfo> = None;
                
                for line in dmi_output.lines() {
                    if line.trim().is_empty() {
                        if let Some(slot) = current_slot.take() {
                            if slot.size_gb > 0.0 {
                                slots.push(slot);
                            }
                        }
                        continue;
                    }
                    
                    if line.contains("Memory Device") && !line.contains("No Module Installed") {
                        current_slot = Some(MemorySlotInfo::default());
                        continue;
                    }
                    
                    if line.contains("No Module Installed") {
                        current_slot = None;
                        continue;
                    }
                    
                    if let Some(slot) = current_slot.as_mut() {
                        let parts: Vec<&str> = line.split(':').collect();
                        if parts.len() != 2 {
                            continue;
                        }
                        
                        let key = parts[0].trim();
                        let value = parts[1].trim();
                        
                        match key {
                            "Locator" => slot.slot = value.to_string(),
                            "Bank Locator" => slot.bank = value.to_string(),
                            "Size" => {
                                if let Some((size_str, unit)) = value.split_whitespace().collect::<Vec<_>>().split_first() {
                                    if let Ok(size) = size_str.parse::<f32>() {
                                        let factor = match unit.to_lowercase().as_str() {
                                            "mb" => 1.0/1024.0,
                                            "gb" => 1.0,
                                            "tb" => 1024.0,
                                            _ => 1.0,
                                        };
                                        slot.size_gb = size * factor;
                                    }
                                }
                            },
                            "Type" => slot.memory_type = value.to_string(),
                            "Speed" => {
                                if let Some(speed_str) = value.split_whitespace().next() {
                                    if let Ok(speed) = speed_str.parse::<u32>() {
                                        slot.speed_mhz = speed;
                                    }
                                }
                            },
                            "Configured Memory Speed" => {
                                if let Some(speed_str) = value.split_whitespace().next() {
                                    if let Ok(speed) = speed_str.parse::<u32>() {
                                        slot.configured_speed_mhz = speed;
                                    }
                                }
                            },
                            "Manufacturer" => slot.manufacturer = value.to_string(),
                            "Part Number" => slot.part_number = value.trim().to_string(),
                            "Serial Number" => slot.serial_number = value.to_string(),
                            "Form Factor" => slot.form_factor = value.to_string(),
                            "Rank" => slot.rank = value.to_string(),
                            "Configured Voltage" => {
                                if let Some(volt_str) = value.split_whitespace().next() {
                                    if let Ok(volt) = volt_str.parse::<f32>() {
                                        slot.timing.voltage_v = volt;
                                    }
                                }
                            },
                            "CAS Latency" => {
                                if let Ok(cl) = value.parse::<u32>() {
                                    slot.timing.cl = cl;
                                }
                            },
                            _ => {}
                        }
                    }
                }
                
                // Add last slot if exists
                if let Some(slot) = current_slot {
                    if slot.size_gb > 0.0 {
                        slots.push(slot);
                    }
                }
            }
        }
        
        // Try to read memory temperatures from jc42 sensors
        let hwmon_base = "/sys/class/hwmon";
        if Path::new(hwmon_base).exists() {
            if let Ok(paths) = fs::read_dir(hwmon_base) {
                let mut dimm_temps = Vec::new();
                
                for entry in paths {
                    if let Ok(entry) = entry {
                        let path = entry.path();
                        if !path.is_dir() {
                            continue;
                        }
                        
                        let name_path = path.join("name");
                        if name_path.exists() {
                            if let Ok(name) = self.read_file_to_string(name_path.to_str().unwrap_or("")) {
                                if name.trim() == "jc42" {
                                    let temp_path = path.join("temp1_input");
                                    if temp_path.exists() {
                                        if let Ok(temp_str) = self.read_file_to_string(temp_path.to_str().unwrap_or("")) {
                                            if let Ok(temp) = temp_str.trim().parse::<f32>() {
                                                dimm_temps.push(temp / 1000.0);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                
                // Apply temperatures to slots
                for (i, slot) in slots.iter_mut().enumerate() {
                    if i < dimm_temps.len() {
                        slot.temperature_c = dimm_temps[i];
                    }
                }
            }
        }
        
        Ok(slots)
    }
    
    fn collect_gpus(&self) -> Result<Vec<GpuInfo>> {
        let mut gpus = Vec::new();
        
        // Try NVIDIA via nvidia-smi
        let output = Command::new("nvidia-smi")
            .args(&[
                "--query-gpu=index,name,memory.total,memory.used,driver_version,temperature.gpu,temperature.memory,clocks.current.graphics,clocks.current.memory,clocks.current.sm,power.draw,power.limit,fan.speed,utilization.gpu,utilization.nvenc,utilization.nvdec,pcie.link.width.current,pcie.link.gen.current,vbios_version",
                "--format=csv,noheader,nounits"
            ])
            .output();
            
        if let Ok(output) = output {
            if output.status.success() {
                let nvidia_output = String::from_utf8_lossy(&output.stdout);
                
                for line in nvidia_output.lines() {
                    let parts: Vec<&str> = line.split(',').collect();
                    if parts.len() < 19 {
                        continue;
                    }
                    
                    let parse_float = |s: &str| s.trim().parse::<f32>().unwrap_or(0.0);
                    let parse_int = |s: &str| s.trim().parse::<u32>().unwrap_or(0);
                    
                    let gpu = GpuInfo {
                        index: parse_int(parts[0]),
                        vendor: "NVIDIA".to_string(),
                        model: parts[1].trim().to_string(),
                        vram_gb: parse_float(parts[2]) / 1024.0,
                        vram_used_gb: parse_float(parts[3]) / 1024.0,
                        driver_version: parts[4].trim().to_string(),
                        temperature_c: parse_float(parts[5]),
                        vram_temp_c: parse_float(parts[6]),
                        core_clock_mhz: parse_float(parts[7]),
                        memory_clock_mhz: parse_float(parts[8]),
                        shader_clock_mhz: parse_float(parts[9]),
                        power_w: parse_float(parts[10]),
                        power_limit_w: parse_float(parts[11]),
                        fan_percent: parse_float(parts[12]),
                        utilization_percent: parse_float(parts[13]),
                        nvenc_usage_percent: parse_float(parts[14]),
                        nvdec_usage_percent: parse_float(parts[15]),
                        pcie_width: parse_int(parts[16]),
                        pcie_gen: parse_int(parts[17]),
                        bios_version: if parts.len() > 18 { parts[18].trim().to_string() } else { String::new() },
                        ..Default::default()
                    };
                    
                    gpus.push(gpu);
                }
            }
        }
        
        // If no NVIDIA GPUs found, try AMD
        if gpus.is_empty() {
            gpus.extend(self.collect_amd_gpus()?);
        }
        
        Ok(gpus)
    }
    
    fn collect_amd_gpus(&self) -> Result<Vec<GpuInfo>> {
        let mut gpus = Vec::new();
        let hwmon_base = "/sys/class/hwmon";
        
        if !Path::new(hwmon_base).exists() {
            return Ok(gpus);
        }
        
        let mut idx = 0;
        
        if let Ok(paths) = fs::read_dir(hwmon_base) {
            for entry in paths {
                if let Ok(entry) = entry {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    
                    let name_path = path.join("name");
                    if !name_path.exists() {
                        continue;
                    }
                    
                    let name = self.read_file_to_string(name_path.to_str().unwrap_or(""))?
                        .trim()
                        .to_string();
                    
                    if name != "amdgpu" {
                        continue;
                    }
                    
                    let mut gpu = GpuInfo {
                        index: idx,
                        vendor: "AMD".to_string(),
                        ..Default::default()
                    };
                    
                    // Model from device/product_name or PCI subsystem
                    let device_path = path.join("device");
                    for fname in &["product_name", "label"] {
                        let p = device_path.join(fname);
                        if p.exists() {
                            if let Ok(model) = self.read_file_to_string(p.to_str().unwrap_or("")) {
                                gpu.model = model.trim().to_string();
                                break;
                            }
                        }
                    }
                    
                    // Temperature sensors
                    if let Ok(temp_entries) = fs::read_dir(&path) {
                        for temp_entry in temp_entries {
                            if let Ok(temp_entry) = temp_entry {
                                let temp_path = temp_entry.path();
                                if !temp_path.file_name().unwrap_or_default().to_string_lossy().ends_with("_input") {
                                    continue;
                                }
                                
                                let label_path = temp_path.with_file_name(
                                    temp_path.file_name().unwrap_or_default().to_string_lossy()
                                        .replace("_input", "_label")
                                );
                                
                                let label = if label_path.exists() {
                                    self.read_file_to_string(label_path.to_str().unwrap_or(""))?
                                        .trim()
                                        .to_lowercase()
                                } else {
                                    String::new()
                                };
                                
                                if let Ok(temp_str) = self.read_file_to_string(temp_path.to_str().unwrap_or("")) {
                                    if let Ok(temp) = temp_str.trim().parse::<f32>() {
                                        let temp_c = temp / 1000.0;
                                        
                                        if label.contains("edge") || !label.contains("junction") {
                                            gpu.temperature_c = temp_c;
                                        } else if label.contains("junction") || label.contains("hotspot") {
                                            gpu.hotspot_temp_c = temp_c;
                                        } else if label.contains("mem") {
                                            gpu.vram_temp_c = temp_c;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    
                    // Power
                    let power_path = device_path.join("power1_average");
                    if power_path.exists() {
                        if let Ok(power_str) = self.read_file_to_string(power_path.to_str().unwrap_or("")) {
                            if let Ok(power) = power_str.trim().parse::<f32>() {
                                gpu.power_w = power / 1_000_000.0;
                            }
                        }
                    }
                    
                    // Fan
                    let fan_path = path.join("fan1_input");
                    if fan_path.exists() {
                        if let Ok(fan_str) = self.read_file_to_string(fan_path.to_str().unwrap_or("")) {
                            if let Ok(fan_rpm) = fan_str.trim().parse::<u32>() {
                                gpu.fan_rpm = fan_rpm;
                            }
                        }
                    }
                    
                    // GPU clock via pp_dpm_sclk
                    let sclk_path = device_path.join("pp_dpm_sclk");
                    if sclk_path.exists() {
                        if let Ok(sclk_content) = self.read_file_to_string(sclk_path.to_str().unwrap_or("")) {
                            for line in sclk_content.lines() {
                                if line.contains("*") {
                                    if let Some(freq_str) = line.split_whitespace().find(|s| s.contains("Mhz")) {
                                        if let Some(freq_num) = freq_str.replace("Mhz", "").parse::<f32>().ok() {
                                            gpu.core_clock_mhz = freq_num;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    
                    // VRAM
                    let vram_total_path = device_path.join("mem_info_vram_total");
                    let vram_used_path = device_path.join("mem_info_vram_used");
                    
                    if vram_total_path.exists() {
                        if let Ok(vram_str) = self.read_file_to_string(vram_total_path.to_str().unwrap_or("")) {
                            if let Ok(vram) = vram_str.trim().parse::<f32>() {
                                gpu.vram_gb = vram / 1_000_000_000.0;
                            }
                        }
                    }
                    
                    if vram_used_path.exists() {
                        if let Ok(vram_str) = self.read_file_to_string(vram_used_path.to_str().unwrap_or("")) {
                            if let Ok(vram) = vram_str.trim().parse::<f32>() {
                                gpu.vram_used_gb = vram / 1_000_000_000.0;
                            }
                        }
                    }
                    
                    gpus.push(gpu);
                    idx += 1;
                }
            }
        }
        
        Ok(gpus)
    }
    
    fn collect_storage(&self) -> Result<Vec<StorageInfo>> {
        let mut devices = Vec::new();
        let block_path = "/sys/block";
        
        if !Path::new(block_path).exists() {
            return Ok(devices);
        }
        
        let now = self.get_timestamp();
        
        if let Ok(entries) = fs::read_dir(block_path) {
            for entry in entries {
                if let Ok(entry) = entry {
                    let dev_path = entry.path();
                    let name = if let Some(name) = dev_path.file_name() {
                        name.to_string_lossy().to_string()
                    } else {
                        continue;
                    };
                    
                    // Skip loop, ram, dm, sr, fd, zram devices
                    if name.starts_with("loop") || name.starts_with("ram") || 
                       name.starts_with("dm-") || name.starts_with("sr") || 
                       name.starts_with("fd") || name.starts_with("zram") {
                        continue;
                    }
                    
                    let size_path = dev_path.join("size");
                    if !size_path.exists() {
                        continue;
                    }
                    
                    let size_sectors = match self.read_file_to_int(size_path.to_str().unwrap_or("")) {
                        Ok(size) => size,
                        Err(_) => continue,
                    };
                    
                    if size_sectors == 0 {
                        continue;
                    }
                    
                    let mut dev = StorageInfo {
                        device: format!("/dev/{}", name),
                        capacity_bytes: (size_sectors * 512) as u64,
                        ..Default::default()
                    };
                    
                    // Model, serial, rotational, interface
                    let device_path = dev_path.join("device");
                    for (fname, attr) in &[("model", "model"), ("serial", "serial"), 
                                          ("vendor", ""), ("firmware_rev", "firmware")] {
                        let p = device_path.join(fname);
                        if p.exists() {
                            if let Ok(val) = self.read_file_to_string(p.to_str().unwrap_or("")) {
                                let val = val.trim().to_string();
                                match *attr {
                                    "model" => dev.model = val,
                                    "serial" => dev.serial = val,
                                    "firmware" => dev.firmware = val,
                                    _ => {}
                                }
                            }
                        }
                    }
                    
                    let rotational_path = dev_path.join("queue/rotational");
                    if rotational_path.exists() {
                        if let Ok(rot_str) = self.read_file_to_string(rotational_path.to_str().unwrap_or("")) {
                            dev.rotational = rot_str.trim() == "1";
                        }
                    }
                    
                    // Interface detection
                    if name.starts_with("nvme") {
                        dev.interface = "NVMe".to_string();
                    } else {
                        let transport_path = device_path.join("transport");
                        if transport_path.exists() {
                            if let Ok(transport) = self.read_file_to_string(transport_path.to_str().unwrap_or("")) {
                                dev.interface = transport.trim().to_uppercase();
                            }
                        } else {
                            dev.interface = if dev.rotational { "HDD/SATA".to_string() } else { "SATA".to_string() };
                        }
                    }
                    
                    // Read/write rates from /sys/block/*/stat
                    let stat_path = dev_path.join("stat");
                    if stat_path.exists() {
                        if let Ok(stat_content) = self.read_file_to_string(stat_path.to_str().unwrap_or("")) {
                            let stat_parts: Vec<&str> = stat_content.trim().split_whitespace().collect();
                            if stat_parts.len() >= 7 {
                                if let (Ok(r_sectors), Ok(w_sectors)) = (
                                    stat_parts[2].parse::<u64>(),
                                    stat_parts[6].parse::<u64>()
                                ) {
                                    if let Some(prev) = self.prev_disk.get(&name) {
                                        let (pr, pw, pt) = *prev;
                                        let dt = now - pt;
                                        if dt > 0.0 {
                                            dev.read_rate_mbs = ((r_sectors - pr) * 512) as f32 / 1_000_000.0 / dt as f32;
                                            dev.write_rate_mbs = ((w_sectors - pw) * 512) as f32 / 1_000_000.0 / dt as f32;
                                        }
                                    }
                                    self.prev_disk.insert(name.clone(), (r_sectors, w_sectors, now));
                                }
                            }
                        }
                    }
                    
                    // SMART data
                    dev.smart = self.collect_smart(&name, &dev.interface)?;
                    if let Some(ref smart) = dev.smart {
                        dev.temperature_c = smart.temperature_c;
                    }
                    
                    devices.push(dev);
                }
            }
        }
        
        Ok(devices)
    }
    
    fn collect_smart(&self, device: &str, interface: &str) -> Result<Option<SmartData>> {
        let output = Command::new("smartctl")
            .args(&["-x", "--json=c", &format!("/dev/{}", device)])
            .output();
            
        if let Ok(output) = output {
            if !output.status.success() && !output.status.code().unwrap_or(1) & !0x01 != 0 {
                return Ok(None);
            }
            
            let smart_output = String::from_utf8_lossy(&output.stdout);
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(&smart_output) {
                let mut smart = SmartData::default();
                
                // Overall health
                if let Some(status) = data.get("smart_status") {
                    if let Some(passed) = status.get("passed") {
                        if passed.as_bool().unwrap_or(true) {
                            smart.overall_status = "PASSED".to_string();
                        } else {
                            smart.overall_status = "FAILED".to_string();
                        }
                    }
                }
                
                // Power-on hours
                if let Some(poh) = data.get("power_on_time") {
                    if let Some(hours) = poh.get("hours") {
                        smart.power_on_hours = hours.as_u64().unwrap_or(0);
                    }
                }
                
                // Temperature
                if let Some(temp) = data.get("temperature") {
                    if let Some(current) = temp.get("current") {
                        smart.temperature_c = current.as_f64().unwrap_or(0.0) as f32;
                    }
                }
                
                // ATA attributes
                if let Some(ata_attrs) = data.get("ata_smart_attributes") {
                    if let Some(table) = ata_attrs.get("table") {
                        if let Some(attrs) = table.as_array() {
                            for attr in attrs {
                                let id = attr.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                let name = attr.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let value = attr.get("value").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                let worst = attr.get("worst").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                let raw = attr.get("raw").and_then(|v| v.get("value")).and_then(|v| v.as_u64()).unwrap_or(0);
                                let threshold = attr.get("thresh").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                let flags = attr.get("flags").and_then(|v| v.get("value")).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                let pre_fail = attr.get("flags").and_then(|v| v.get("prefailure")).and_then(|v| v.as_bool()).unwrap_or(false);
                                
                                let sa = SmartAttribute {
                                    id,
                                    name,
                                    value,
                                    worst,
                                    raw,
                                    threshold,
                                    flags,
                                    pre_fail,
                                    failing: value != 0 && value <= threshold && pre_fail,
                                };
                                
                                smart.attributes.push(sa);
                                
                                // Extract key attrs
                                if id == 5 {
                                    smart.reallocated_sectors = raw;
                                } else if id == 197 {
                                    smart.pending_sectors = raw;
                                } else if id == 198 {
                                    smart.uncorrectable_errors = raw;
                                }
                            }
                        }
                    }
                }
                
                // Calculate health from reallocated/pending/uncorrectable
                let penalty = (smart.reallocated_sectors * 2 +
                              smart.pending_sectors * 2 +
                              smart.uncorrectable_errors * 5) as u32;
                smart.health_percent = 100u32.saturating_sub(penalty.min(100));
                
                // NVMe data
                if let Some(nvme_health) = data.get("nvme_smart_health_information_log") {
                    let nvme = NvmeData {
                        temperature_c: smart.temperature_c,
                        available_spare_percent: nvme_health.get("available_spare").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                        available_spare_threshold_percent: nvme_health.get("available_spare_threshold").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                        percentage_used: nvme_health.get("percentage_used").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                        data_units_written: nvme_health.get("data_units_written").and_then(|v| v.as_u64()).unwrap_or(0),
                        data_units_read: nvme_health.get("data_units_read").and_then(|v| v.as_u64()).unwrap_or(0),
                        host_write_commands: nvme_health.get("host_write_commands").and_then(|v| v.as_u64()).unwrap_or(0),
                        host_read_commands: nvme_health.get("host_read_commands").and_then(|v| v.as_u64()).unwrap_or(0),
                        media_errors: nvme_health.get("media_errors").and_then(|v| v.as_u64()).unwrap_or(0),
                        num_err_log_entries: nvme_health.get("num_err_log_entries").and_then(|v| v.as_u64()).unwrap_or(0),
                        power_on_hours: smart.power_on_hours,
                        unsafe_shutdowns: nvme_health.get("unsafe_shutdowns").and_then(|v| v.as_u64()).unwrap_or(0),
                        critical_warning: nvme_health.get("critical_warning").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                    };
                    
                    smart.nvme = Some(nvme);
                    smart.health_percent = 100u32.saturating_sub(nvme.percentage_used);
                    
                    if nvme.critical_warning != 0 {
                        smart.overall_status = "FAILED".to_string();
                    }
                }
                
                return Ok(Some(smart));
            }
        }
        
        Ok(None)
    }
    
    fn collect_motherboard(&self) -> Result<MotherboardInfo> {
        let mut mb = MotherboardInfo::default();
        
        // DMI fields
        let dmi_fields = [
            ("/sys/class/dmi/id/board_vendor", "manufacturer"),
            ("/sys/class/dmi/id/board_name", "model"),
            ("/sys/class/dmi/id/board_version", "version"),
            ("/sys/class/dmi/id/bios_vendor", "bios_vendor"),
            ("/sys/class/dmi/id/bios_version", "bios_version"),
            ("/sys/class/dmi/id/bios_date", "bios_date"),
        ];
        
        for (path, attr) in &dmi_fields {
            if Path::new(path).exists() {
                if let Ok(content) = self.read_file_to_string(path) {
                    let content = content.trim().to_string();
                    match *attr {
                        "manufacturer" => mb.manufacturer = content,
                        "model" => mb.model = content,
                        "version" => mb.version = content,
                        "bios_vendor" => mb.bios_vendor = content,
                        "bios_version" => mb.bios_version = content,
                        "bios_date" => mb.bios_date = content,
                        _ => {}
                    }
                }
            }
        }
        
        // Chipset from lspci
        let output = Command::new("lspci")
            .args(&["-mm", "-d", "::0600"])
            .output();
            
        if let Ok(output) = output {
            if output.status.success() && !output.stdout.is_empty() {
                let lspci_output = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = lspci_output.lines().next() {
                    // Parse format: "00:00.0 "Host bridge" "Intel Corporation" "..."
                    let parts: Vec<&str> = first_line.split('"').collect();
                    if parts.len() >= 5 {
                        mb.chipset = format!("{} {}", parts[3], parts[4]).trim().to_string();
                    }
                }
            }
        }
        
        // VRM / PCH temperatures from hwmon
        let hwmon_base = "/sys/class/hwmon";
        if Path::new(hwmon_base).exists() {
            if let Ok(paths) = fs::read_dir(hwmon_base) {
                for entry in paths {
                    if let Ok(entry) = entry {
                        let path = entry.path();
                        if !path.is_dir() {
                            continue;
                        }
                        
                        let name_path = path.join("name");
                        if !name_path.exists() {
                            continue;
                        }
                        
                        let valid_names = [
                            "nct6775", "nct6779", "nct6776", "nct6791",
                            "nct6792", "nct6793", "nct6795", "nct6796",
                            "nct6797", "nct6798", "asus_ec", "it8720f",
                            "it8728f", "it8771e", "w83627ehf"
                        ];
                        
                        let name = self.read_file_to_string(name_path.to_str().unwrap_or(""))?
                            .trim()
                            .to_string();
                        
                        if !valid_names.contains(&name.as_str()) {
                            continue;
                        }
                        
                        // Check temperature inputs
                        if let Ok(temp_entries) = fs::read_dir(&path) {
                            for temp_entry in temp_entries {
                                if let Ok(temp_entry) = temp_entry {
                                    let temp_path = temp_entry.path();
                                    if !temp_path.file_name().unwrap_or_default().to_string_lossy().ends_with("_input") {
                                        continue;
                                    }
                                    
                                    let label_path = temp_path.with_file_name(
                                        temp_path.file_name().unwrap_or_default().to_string_lossy()
                                            .replace("_input", "_label")
                                    );
                                    
                                    let label = if label_path.exists() {
                                        self.read_file_to_string(label_path.to_str().unwrap_or(""))?
                                            .trim()
                                            .to_lowercase()
                                    } else {
                                        String::new()
                                    };
                                    
                                    if let Ok(temp_str) = self.read_file_to_string(temp_path.to_str().unwrap_or("")) {
                                        if let Ok(temp) = temp_str.trim().parse::<f32>() {
                                            let temp_c = temp / 1000.0;
                                            let ll = label.to_lowercase();
                                            
                                            if ll.contains("vrm") || ll.contains("vcore") {
                                                mb.vrm_temp_c = temp_c;
                                            } else if ll.contains("pch") {
                                                mb.pch_temp_c = temp_c;
                                            } else if ll.contains("ambient") || ll.contains("system") || ll.contains("board") {
                                                if mb.ambient_temp_c == 0.0 {
                                                    mb.ambient_temp_c = temp_c;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        
        Ok(mb)
    }
    
    fn collect_hwmon_fans_voltages(&self) -> Result<(Vec<FanReading>, Vec<VoltageReading>)> {
        let mut fans = Vec::new();
        let mut voltages = Vec::new();
        let hwmon_base = "/sys/class/hwmon";
        
        if !Path::new(hwmon_base).exists() {
            return Ok((fans, voltages));
        }
        
        if let Ok(paths) = fs::read_dir(hwmon_base) {
            for entry in paths {
                if let Ok(entry) = entry {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    
                    let name_path = path.join("name");
                    if !name_path.exists() {
                        continue;
                    }
                    
                    let chip_name = self.read_file_to_string(name_path.to_str().unwrap_or(""))?
                        .trim()
                        .to_string();
                    
                    // Fan inputs
                    if let Ok(fan_entries) = fs::read_dir(&path) {
                        for fan_entry in fan_entries {
                            if let Ok(fan_entry) = fan_entry {
                                let fan_path = fan_entry.path();
                                if !fan_path.file_name().unwrap_or_default().to_string_lossy().ends_with("_input") {
                                    continue;
                                }
                                
                                let file_name = fan_path.file_name().unwrap_or_default().to_string_lossy();
                                if let Some(n) = file_name.strip_suffix("_input").and_then(|s| s.strip_prefix("fan")) {
                                    let label_path = path.join(format!("fan{}_label", n));
                                    let label = if label_path.exists() {
                                        self.read_file_to_string(label_path.to_str().unwrap_or(""))?
                                            .trim()
                                            .to_string()
                                    } else {
                                        format!("{} Fan{}", chip_name, n)
                                    };
                                    
                                    if let Ok(rpm_str) = self.read_file_to_string(fan_path.to_str().unwrap_or("")) {
                                        if let Ok(rpm) = rpm_str.trim().parse::<u32>() {
                                            if rpm > 0 && rpm <= 20000 {
                                                // Target RPM for controllable fans
                                                let target_path = path.join(format!("fan{}_target", n));
                                                let target = if target_path.exists() {
                                                    self.read_file_to_int(target_path.to_str().unwrap_or(""))
                                                        .unwrap_or(0) as u32
                                                } else {
                                                    0
                                                };
                                                
                                                fans.push(FanReading {
                                                    name: label,
                                                    rpm,
                                                    percent: 0.0,
                                                    target_rpm: target,
                                                    controllable: target_path.exists(),
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    
                    // Voltage inputs
                    if let Ok(volt_entries) = fs::read_dir(&path) {
                        for volt_entry in volt_entries {
                            if let Ok(volt_entry) = volt_entry {
                                let volt_path = volt_entry.path();
                                if !volt_path.file_name().unwrap_or_default().to_string_lossy().ends_with("_input") {
                                    continue;
                                }
                                
                                let file_name = volt_path.file_name().unwrap_or_default().to_string_lossy();
                                if let Some(n) = file_name.strip_suffix("_input").and_then(|s| s.strip_prefix("in")) {
                                    let label_path = path.join(format!("in{}_label", n));
                                    let label = if label_path.exists() {
                                        self.read_file_to_string(label_path.to_str().unwrap_or(""))?
                                            .trim()
                                            .to_string()
                                    } else {
                                        format!("{} V{}", chip_name, n)
                                    };
                                    
                                    if let Ok(mv_str) = self.read_file_to_string(volt_path.to_str().unwrap_or("")) {
                                        if let Ok(mv) = mv_str.trim().parse::<f32>() {
                                            let v = mv / 1000.0;
                                            if v > 0.0 && v <= 25.0 {
                                                let min_v = {
                                                    let min_path = path.join(format!("in{}_min", n));
                                                    if min_path.exists() {
                                                        self.read_file_to_string(min_path.to_str().unwrap_or(""))
                                                            .ok()
                                                            .and_then(|s| s.trim().parse::<f32>().ok())
                                                            .map(|v| v / 1000.0)
                                                            .unwrap_or(0.0)
                                                    } else {
                                                        0.0
                                                    }
                                                };
                                                
                                                let max_v = {
                                                    let max_path = path.join(format!("in{}_max", n));
                                                    if max_path.exists() {
                                                        self.read_file_to_string(max_path.to_str().unwrap_or(""))
                                                            .ok()
                                                            .and_then(|s| s.trim().parse::<f32>().ok())
                                                            .map(|v| v / 1000.0)
                                                            .unwrap_or(0.0)
                                                    } else {
                                                        0.0
                                                    }
                                                };
                                                
                                                voltages.push(VoltageReading {
                                                    name: label,
                                                    voltage_v: v,
                                                    min_v,
                                                    max_v,
                                                    nominal_v: 0.0,
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        
        Ok((fans, voltages))
    }
    
    fn collect_batteries(&self) -> Result<Vec<BatteryInfo>> {
        let mut bats = Vec::new();
        let ps_base = "/sys/class/power_supply";
        
        if !Path::new(ps_base).exists() {
            return Ok(bats);
        }
        
        if let Ok(entries) = fs::read_dir(ps_base) {
            for entry in entries {
                if let Ok(entry) = entry {
                    let ps_path = entry.path();
                    
                    let type_path = ps_path.join("type");
                    if !type_path.exists() {
                        continue;
                    }
                    
                    let ps_type = self.read_file_to_string(type_path.to_str().unwrap_or(""))?
                        .trim()
                        .to_string();
                    
                    if ps_type != "Battery" {
                        continue;
                    }
                    
                    let name = if let Some(name) = ps_path.file_name() {
                        name.to_string_lossy().to_string()
                    } else {
                        continue;
                    };
                    
                    let mut bat = BatteryInfo {
                        name,
                        ..Default::default()
                    };
                    
                    let read_int = |fname: &str| -> i64 {
                        let p = ps_path.join(fname);
                        self.read_file_to_int(p.to_str().unwrap_or(""))
                            .unwrap_or(0)
                    };
                    
                    let read_string = |fname: &str| -> String {
                        let p = ps_path.join(fname);
                        self.read_file_to_string(p.to_str().unwrap_or(""))
                            .unwrap_or_default()
                            .trim()
                            .to_string()
                    };
                    
                    bat.status = read_string("status");
                    bat.technology = read_string("technology");
                    bat.cycle_count = read_int("cycle_count") as u32;
                    bat.voltage_mv = (read_int("voltage_now") / 1000) as u32;
                    
                    // Energy units (preferred) or charge units
                    if ps_path.join("energy_full_design").exists() {
                        bat.capacity_design_mwh = (read_int("energy_full_design") / 1000) as u32;
                        bat.capacity_full_mwh = (read_int("energy_full") / 1000) as u32;
                        bat.capacity_now_mwh = (read_int("energy_now") / 1000) as u32;
                        bat.charge_rate_mw = (read_int("power_now") / 1000) as u32;
                    } else if ps_path.join("charge_full_design").exists() {
                        // Convert charge (µAh) to energy (mWh) using voltage
                        let v_uv = read_int("voltage_now").max(3_700_000);
                        bat.capacity_design_mwh = (read_int("charge_full_design") * v_uv / 1_000_000_000) as u32;
                        bat.capacity_full_mwh = (read_int("charge_full") * v_uv / 1_000_000_000) as u32;
                        bat.capacity_now_mwh = (read_int("charge_now") * v_uv / 1_000_000_000) as u32;
                        bat.charge_rate_mw = (read_int("current_now") * v_uv / 1_000_000_000) as u32;
                    }
                    
                    bats.push(bat);
                }
            }
        }
        
        Ok(bats)
    }
    
    fn collect_pcie(&self) -> Result<Vec<PcieDevice>> {
        let mut devices = Vec::new();
        
        let output = Command::new("lspci")
            .args(&["-vmm"])
            .output();
            
        if let Ok(output) = output {
            if output.status.success() {
                let lspci_output = String::from_utf8_lossy(&output.stdout);
                let mut current = std::collections::HashMap::new();
                
                for line in lspci_output.lines() {
                    if line.trim().is_empty() {
                        if !current.is_empty() {
                            let d = PcieDevice {
                                slot: current.get("Slot").cloned().unwrap_or_default(),
                                class_name: current.get("Class").cloned().unwrap_or_default(),
                                vendor: current.get("Vendor").cloned().unwrap_or_default(),
                                device: current.get("Device").cloned().unwrap_or_default(),
                                subsystem: current.get("SDevice").cloned().unwrap_or_default(),
                                driver: current.get("Driver").cloned().unwrap_or_default(),
                                ..Default::default()
                            };
                            
                            // Check PCIe link width for this device
                            let slot_id = d.slot.replace(":", "/").replace(".", "/");
                            let link_path = format!("/sys/bus/pci/devices/0000:{}", d.slot);
                            let link_path = Path::new(&link_path);
                            
                            if link_path.exists() {
                                let width_path = link_path.join("current_link_width");
                                let gen_path = link_path.join("current_link_speed");
                                
                                if width_path.exists() {
                                    if let Ok(width_str) = self.read_file_to_string(width_path.to_str().unwrap_or("")) {
                                        if let Ok(width) = width_str.trim().parse::<u32>() {
                                            d.width = width;
                                        }
                                    }
                                }
                                
                                if gen_path.exists() {
                                    if let Ok(gen_str) = self.read_file_to_string(gen_path.to_str().unwrap_or("")) {
                                        if let Some(gen_num) = gen_str.trim().split('.').next() {
                                            if let Ok(gen) = gen_num.parse::<u32>() {
                                                d.gen = gen;
                                            }
                                        }
                                    }
                                }
                            }
                            
                            devices.push(d);
                            current.clear();
                        }
                    } else if let Some((k, v)) = line.split_once(':') {
                        current.insert(k.trim().to_string(), v.trim().to_string());
                    }
                }
            }
        }
        
        Ok(devices)
    }
    
    fn collect_network(&self) -> Result<Vec<NetworkInfo>> {
        let mut nics = Vec::new();
        let net_base = "/sys/class/net";
        
        if !Path::new(net_base).exists() {
            return Ok(nics);
        }
        
        let now = self.get_timestamp();
        
        if let Ok(entries) = fs::read_dir(net_base) {
            for entry in entries {
                if let Ok(entry) = entry {
                    let iface_path = entry.path();
                    let name = if let Some(name) = iface_path.file_name() {
                        name.to_string_lossy().to_string()
                    } else {
                        continue;
                    };
                    
                    if name == "lo" {
                        continue;
                    }
                    
                    let mut nic = NetworkInfo {
                        name: name.clone(),
                        ..Default::default()
                    };
                    
                    // MAC
                    let addr_path = iface_path.join("address");
                    if addr_path.exists() {
                        if let Ok(mac) = self.read_file_to_string(addr_path.to_str().unwrap_or("")) {
                            nic.mac = mac.trim().to_string();
                        }
                    }
                    
                    // Speed
                    let speed_path = iface_path.join("speed");
                    if speed_path.exists() {
                        if let Ok(speed_str) = self.read_file_to_string(speed_path.to_str().unwrap_or("")) {
                            if let Ok(speed) = speed_str.trim().parse::<u32>() {
                                nic.speed_mbps = speed;
                            }
                        }
                    }
                    
                    // Link state
                    let state_path = iface_path.join("operstate");
                    if state_path.exists() {
                        if let Ok(state) = self.read_file_to_string(state_path.to_str().unwrap_or("")) {
                            nic.link_up = state.trim() == "up";
                        }
                    }
                    
                    // Driver
                    let driver_link = iface_path.join("device/driver");
                    if driver_link.is_symlink() {
                        if let Ok(driver_path) = fs::read_link(&driver_link) {
                            if let Some(driver_name) = driver_path.file_name() {
                                nic.driver = driver_name.to_string_lossy().to_string();
                            }
                        }
                    }
                    
                    // RX/TX bytes and rates
                    let rx_path = iface_path.join("statistics/rx_bytes");
                    let tx_path = iface_path.join("statistics/tx_bytes");
                    let rx_err_path = iface_path.join("statistics/rx_errors");
                    let tx_err_path = iface_path.join("statistics/tx_errors");
                    
                    let rx = self.read_file_to_int(rx_path.to_str().unwrap_or("")).unwrap_or(0) as u64;
                    let tx = self.read_file_to_int(tx_path.to_str().unwrap_or("")).unwrap_or(0) as u64;
                    let rx_err = self.read_file_to_int(rx_err_path.to_str().unwrap_or("")).unwrap_or(0) as u64;
                    let tx_err = self.read_file_to_int(tx_err_path.to_str().unwrap_or("")).unwrap_or(0) as u64;
                    
                    nic.rx_bytes = rx;
                    nic.tx_bytes = tx;
                    nic.rx_errors = rx_err;
                    nic.tx_errors = tx_err;
                    
                    if let Some(prev) = self.prev_net.get(&name) {
                        let (prx, ptx, pt) = *prev;
                        let dt = now - pt;
                        if dt > 0.0 {
                            nic.rx_rate_mbs = ((rx - prx) as f32) / 1_000_000.0 / dt as f32;
                            nic.tx_rate_mbs = ((tx - ptx) as f32) / 1_000_000.0 / dt as f32;
                        }
                    }
                    
                    self.prev_net.insert(name, (rx, tx, now));
                    nics.push(nic);
                }
            }
        }
        
        Ok(nics)
    }
}

// Helper functions
fn get_hostname() -> String {
    hostname::get().map(|h| h.to_string_lossy().to_string()).unwrap_or_else(|_| "unknown".to_string())
}

fn get_os_version() -> String {
    if let Ok(release) = fs::read_to_string("/proc/version") {
        release.trim().to_string()
    } else {
        "unknown".to_string()
    }
}

// Main hardware info collector
pub struct HardwareInfoCollector {
    collector: Box<dyn HardwareCollector>,
}

impl HardwareInfoCollector {
    pub fn new() -> Result<Self> {
        let collector: Box<dyn HardwareCollector> = if cfg!(target_os = "linux") {
            Box::new(LinuxHardwareCollector::new())
        } else {
            return Err(anyhow!("Unsupported platform"));
        };
        
        Ok(Self { collector })
    }
    
    pub fn snapshot(&self) -> Result<HardwareSnapshot> {
        self.collector.collect()
    }
}
