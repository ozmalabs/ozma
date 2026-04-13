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
use serde::{Deserialize, Serialize};
use anyhow::Result;

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
        // Implementation will be added in subsequent steps
        Ok(CpuPackageInfo::default())
    }
    
    fn collect_memory(&self) -> Result<Vec<MemorySlotInfo>> {
        // Implementation will be added in subsequent steps
        Ok(Vec::new())
    }
    
    fn collect_gpus(&self) -> Result<Vec<GpuInfo>> {
        // Implementation will be added in subsequent steps
        Ok(Vec::new())
    }
    
    fn collect_storage(&self) -> Result<Vec<StorageInfo>> {
        // Implementation will be added in subsequent steps
        Ok(Vec::new())
    }
    
    fn collect_motherboard(&self) -> Result<MotherboardInfo> {
        // Implementation will be added in subsequent steps
        Ok(MotherboardInfo::default())
    }
    
    fn collect_hwmon_fans_voltages(&self) -> Result<(Vec<FanReading>, Vec<VoltageReading>)> {
        // Implementation will be added in subsequent steps
        Ok((Vec::new(), Vec::new()))
    }
    
    fn collect_batteries(&self) -> Result<Vec<BatteryInfo>> {
        // Implementation will be added in subsequent steps
        Ok(Vec::new())
    }
    
    fn collect_pcie(&self) -> Result<Vec<PcieDevice>> {
        // Implementation will be added in subsequent steps
        Ok(Vec::new())
    }
    
    fn collect_network(&self) -> Result<Vec<NetworkInfo>> {
        // Implementation will be added in subsequent steps
        Ok(Vec::new())
    }
}

// Helper functions
fn get_hostname() -> String {
    hostname::get().map(|h| h.to_string_lossy().to_string()).unwrap_or_else(|_| "unknown".to_string())
}

fn get_os_version() -> String {
    // Implementation will be added in subsequent steps
    "unknown".to_string()
}

// Main hardware info collector
pub struct HardwareInfoCollector {
    collector: Box<dyn HardwareCollector>,
}

impl HardwareInfoCollector {
    pub fn new() -> Result<Self> {
        let collector: Box<dyn HardwareCollector> = if cfg!(target_os = "linux") {
            Box::new(LinuxHardwareCollector::new())
        } else if cfg!(target_os = "windows") {
            // Windows implementation will be added later
            Box::new(LinuxHardwareCollector::new()) // fallback for now
        } else if cfg!(target_os = "macos") {
            // macOS implementation will be added later
            Box::new(LinuxHardwareCollector::new()) // fallback for now
        } else {
            Box::new(LinuxHardwareCollector::new())
        };
        
        Ok(Self { collector })
    }
    
    pub fn snapshot(&self) -> Result<HardwareSnapshot> {
        self.collector.collect()
    }
}
