//! System metrics collection.
//!
//! Gathered every 60 s and pushed to the Connect API.

use serde::Serialize;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

/// Snapshot of host system statistics.
#[derive(Debug, Clone, Serialize)]
pub struct SystemMetrics {
    /// CPU usage across all logical cores, 0.0–100.0.
    pub cpu_usage_pct: f32,
    /// Total physical RAM in bytes.
    pub mem_total_bytes: u64,
    /// Used RAM in bytes.
    pub mem_used_bytes: u64,
    /// Total swap in bytes.
    pub swap_total_bytes: u64,
    /// Used swap in bytes.
    pub swap_used_bytes: u64,
    /// Number of logical CPU cores.
    pub cpu_count: usize,
    /// OS name + version string, e.g. "Linux 6.8.0".
    pub os_version: String,
    /// Hostname.
    pub hostname: String,
}

/// Collect a fresh [`SystemMetrics`] snapshot.
///
/// `sysinfo` requires two refreshes separated by a short interval to produce
/// meaningful CPU percentages; callers that need accurate CPU figures should
/// call this function twice with a ~200 ms sleep between calls, or accept the
/// first call returning 0 % CPU (which is fine for 60-second polling).
pub fn collect() -> SystemMetrics {
    let mut sys = System::new_with_specifics(
        RefreshKind::new()
            .with_cpu(CpuRefreshKind::everything())
            .with_memory(MemoryRefreshKind::everything()),
    );
    sys.refresh_all();

    let cpu_usage_pct = {
        let cpus = sys.cpus();
        if cpus.is_empty() {
            0.0
        } else {
            cpus.iter().map(|c| c.cpu_usage()).sum::<f32>() / cpus.len() as f32
        }
    };

    SystemMetrics {
        cpu_usage_pct,
        mem_total_bytes: sys.total_memory(),
        mem_used_bytes: sys.used_memory(),
        swap_total_bytes: sys.total_swap(),
        swap_used_bytes: sys.used_swap(),
        cpu_count: sys.cpus().len(),
        os_version: System::long_os_version().unwrap_or_default(),
        hostname: hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unknown".into()),
    }
}
