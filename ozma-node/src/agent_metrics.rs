// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Bounded, honest host metrics for `GET /agent/metrics`.
//!
//! `controller/device_metrics.py` polls `/agent/metrics` without a bearer token
//! and feeds the decoded JSON to `MetricSource.update_batch`, which records only
//! **top-level numeric** values. This module therefore returns a flat JSON
//! object whose numeric keys are the ones the controller already understands
//! (`cpu_usage`, `ram_used`, `ram_total`, `disk_used`, `disk_total`,
//! `net_rx_rate`, `net_tx_rate`, `uptime`, plus `cpu_temp`, `gpu_temp`, and
//! `fan_rpm` when hwmon exposes them).
//!
//! Collection is deliberately small: a fixed set of procfs files, each read
//! through a 256 KiB cap, one `statvfs` call, and read-only hwmon sysfs. There is
//! no subprocess, network probe, or unbounded read. A metric that cannot be
//! measured is omitted and named in `unavailable`; it is never reported as a
//! fake zero.
//! Non-numeric metadata (`available`, `scope`, `notes`, ...) is ignored by the
//! controller's numeric batch update and exists so a human can see what the
//! document does and does not claim.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ozma_thermal::HwmonInventory;
use serde_json::{Map, Number, Value};

/// Largest single procfs read accepted, in bytes.
pub const MAX_PROC_BYTES: u64 = 256 * 1024;
/// Shortest interval used to derive a network rate from counter deltas.
const MIN_RATE_INTERVAL: Duration = Duration::from_millis(50);

/// Shared sampler behind the public `/agent/metrics` route.
#[derive(Clone, Debug, Default)]
pub struct AgentMetricsSampler {
    previous: Arc<Mutex<PreviousSample>>,
}

#[derive(Debug, Default)]
struct PreviousSample {
    cpu: Option<CpuTotals>,
    net: Option<NetSample>,
}

#[derive(Clone, Copy, Debug)]
struct NetSample {
    totals: NetTotals,
    at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CpuTotals {
    total: u64,
    idle: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NetTotals {
    rx_bytes: u64,
    tx_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MemoryTotals {
    total_bytes: u64,
    used_bytes: u64,
}

#[derive(Clone, Debug)]
struct MetricsPaths {
    proc_root: PathBuf,
    disk_path: PathBuf,
    hwmon_root: PathBuf,
}

impl MetricsPaths {
    fn host() -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
            disk_path: PathBuf::from("/"),
            hwmon_root: PathBuf::from(ozma_thermal::DEFAULT_HWMON_ROOT),
        }
    }
}

impl AgentMetricsSampler {
    /// Sample the host and build the controller-facing metrics document.
    pub fn collect(&self, node_id: &str) -> Value {
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            return self.collect_sysinfo(node_id);
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        self.collect_with(&MetricsPaths::host(), node_id)
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn collect_sysinfo(&self, node_id: &str) -> Value {
        use sysinfo::{Disks, Networks, System};

        let mut sys = System::new_all();
        sys.refresh_cpu();
        sys.refresh_memory();

        let mut metrics = Map::new();
        let mut unavailable: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();

        let cpu = f64::from(sys.global_cpu_info().cpu_usage());
        if cpu.is_finite() && cpu >= 0.0 {
            metrics.insert("cpu_usage".to_owned(), number(round2(cpu.min(100.0))));
        } else {
            unavailable.push("cpu_usage".to_owned());
        }

        let ram_total = sys.total_memory();
        let ram_used = sys.used_memory();
        if ram_total > 0 {
            metrics.insert("ram_total".to_owned(), Value::from(ram_total));
            metrics.insert("ram_used".to_owned(), Value::from(ram_used));
        } else {
            unavailable.push("ram_used".to_owned());
            unavailable.push("ram_total".to_owned());
        }

        let disks = Disks::new_with_refreshed_list();
        let root_mount = if cfg!(target_os = "windows") {
            Path::new("C:\\")
        } else {
            Path::new("/")
        };
        let (disk_total, disk_used) = disks
            .iter()
            .find(|d| d.mount_point() == root_mount)
            .or_else(|| disks.iter().next())
            .map(|d| {
                (
                    d.total_space(),
                    d.total_space().saturating_sub(d.available_space()),
                )
            })
            .unwrap_or((0, 0));
        if disk_total > 0 {
            metrics.insert("disk_total".to_owned(), Value::from(disk_total));
            metrics.insert("disk_used".to_owned(), Value::from(disk_used));
        } else {
            unavailable.push("disk_used".to_owned());
            unavailable.push("disk_total".to_owned());
        }

        let networks = Networks::new_with_refreshed_list();
        let mut rx_total: u64 = 0;
        let mut tx_total: u64 = 0;
        for (_name, data) in &networks {
            rx_total = rx_total.saturating_add(data.total_received());
            tx_total = tx_total.saturating_add(data.total_transmitted());
        }
        let current = NetTotals {
            rx_bytes: rx_total,
            tx_bytes: tx_total,
        };
        let now = Instant::now();
        let rates = {
            let mut previous = lock(&self.previous);
            let rates = previous
                .net
                .and_then(|sample| net_rates(sample, current, now));
            previous.net = Some(NetSample {
                totals: current,
                at: now,
            });
            rates
        };
        match rates {
            Some((rx, tx)) => {
                metrics.insert("net_rx_rate".to_owned(), number(round2(rx)));
                metrics.insert("net_tx_rate".to_owned(), number(round2(tx)));
            }
            None => {
                unavailable.push("net_rx_rate".to_owned());
                unavailable.push("net_tx_rate".to_owned());
                notes.push("network rates need two samples at least 50 ms apart".to_owned());
            }
        }

        metrics.insert("uptime".to_owned(), Value::from(System::uptime()));

        for missing in ["cpu_temp", "gpu_usage", "gpu_temp", "fan_rpm", "power_draw"] {
            unavailable.push(missing.to_owned());
        }

        let available = !metrics.is_empty();
        let mut document = Map::new();
        document.insert("ok".to_owned(), Value::Bool(true));
        document.insert("available".to_owned(), Value::Bool(available));
        document.insert("source".to_owned(), Value::String("ozma-node".to_owned()));
        document.insert("scope".to_owned(), Value::String("host".to_owned()));
        document.insert("node_id".to_owned(), Value::String(node_id.to_owned()));
        document.insert(
            "unavailable".to_owned(),
            Value::Array(unavailable.into_iter().map(Value::String).collect()),
        );
        document.insert("notes".to_owned(), Value::String(notes.join("; ")));
        document.extend(metrics);
        Value::Object(document)
    }

    fn collect_with(&self, paths: &MetricsPaths, node_id: &str) -> Value {
        let mut metrics = Map::new();
        let mut unavailable: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();

        match read_bounded(&paths.proc_root.join("stat"))
            .ok()
            .and_then(|text| parse_cpu_totals(&text))
        {
            Some(current) => {
                let (percent, window) = {
                    let mut previous = lock(&self.previous);
                    let sample = match previous.cpu {
                        Some(previous_cpu) => match cpu_percent(previous_cpu, current) {
                            Some(percent) => (Some(percent), "since-last-sample"),
                            None => (cpu_percent_since_boot(current), "since-boot"),
                        },
                        None => (cpu_percent_since_boot(current), "since-boot"),
                    };
                    previous.cpu = Some(current);
                    sample
                };
                match percent {
                    Some(percent) => {
                        metrics.insert("cpu_usage".to_owned(), number(round2(percent)));
                        notes.push(format!("cpu_usage window: {window}"));
                    }
                    None => unavailable.push("cpu_usage".to_owned()),
                }
            }
            None => unavailable.push("cpu_usage".to_owned()),
        }

        match read_bounded(&paths.proc_root.join("meminfo"))
            .ok()
            .and_then(|text| parse_memory(&text))
        {
            Some(memory) => {
                metrics.insert("ram_total".to_owned(), Value::from(memory.total_bytes));
                metrics.insert("ram_used".to_owned(), Value::from(memory.used_bytes));
            }
            None => {
                unavailable.push("ram_used".to_owned());
                unavailable.push("ram_total".to_owned());
            }
        }

        match disk_usage(&paths.disk_path) {
            Some((total, used)) => {
                metrics.insert("disk_total".to_owned(), Value::from(total));
                metrics.insert("disk_used".to_owned(), Value::from(used));
            }
            None => {
                unavailable.push("disk_used".to_owned());
                unavailable.push("disk_total".to_owned());
            }
        }

        let now = Instant::now();
        match read_bounded(&paths.proc_root.join("net/dev"))
            .ok()
            .and_then(|text| parse_net_totals(&text))
        {
            Some(current) => {
                let rates = {
                    let mut previous = lock(&self.previous);
                    let rates = previous
                        .net
                        .and_then(|sample| net_rates(sample, current, now));
                    previous.net = Some(NetSample {
                        totals: current,
                        at: now,
                    });
                    rates
                };
                match rates {
                    Some((rx, tx)) => {
                        metrics.insert("net_rx_rate".to_owned(), number(round2(rx)));
                        metrics.insert("net_tx_rate".to_owned(), number(round2(tx)));
                    }
                    None => {
                        unavailable.push("net_rx_rate".to_owned());
                        unavailable.push("net_tx_rate".to_owned());
                        notes
                            .push("network rates need two samples at least 50 ms apart".to_owned());
                    }
                }
            }
            None => {
                unavailable.push("net_rx_rate".to_owned());
                unavailable.push("net_tx_rate".to_owned());
            }
        }

        match read_bounded(&paths.proc_root.join("uptime"))
            .ok()
            .and_then(|text| parse_uptime(&text))
        {
            Some(seconds) => {
                metrics.insert("uptime".to_owned(), Value::from(seconds));
            }
            None => unavailable.push("uptime".to_owned()),
        }

        let (cpu_temp, gpu_temp, fan_rpm) = thermal_metrics(&paths.hwmon_root);
        if let Some(value) = cpu_temp {
            metrics.insert("cpu_temp".to_owned(), number(round2(value)));
        }
        if let Some(value) = gpu_temp {
            metrics.insert("gpu_temp".to_owned(), number(round2(value)));
        }
        if let Some(value) = fan_rpm {
            metrics.insert("fan_rpm".to_owned(), Value::from(value));
        }
        if cpu_temp.is_some() || gpu_temp.is_some() || fan_rpm.is_some() {
            notes.push("thermal values come from hwmon".to_owned());
        }

        // Metrics this node does not measure at all. Listing them keeps the
        // document honest about why the dashboard has no value.
        for (name, present) in [
            ("cpu_temp", cpu_temp.is_some()),
            ("gpu_usage", false),
            ("gpu_temp", gpu_temp.is_some()),
            ("fan_rpm", fan_rpm.is_some()),
            ("power_draw", false),
        ] {
            if !present {
                unavailable.push(name.to_owned());
            }
        }

        let available = !metrics.is_empty();
        let mut document = Map::new();
        document.insert("ok".to_owned(), Value::Bool(true));
        document.insert("available".to_owned(), Value::Bool(available));
        document.insert("source".to_owned(), Value::String("ozma-node".to_owned()));
        document.insert("scope".to_owned(), Value::String("host".to_owned()));
        document.insert("node_id".to_owned(), Value::String(node_id.to_owned()));
        document.insert(
            "unavailable".to_owned(),
            Value::Array(
                unavailable
                    .into_iter()
                    .map(Value::String)
                    .collect::<Vec<_>>(),
            ),
        );
        document.insert("notes".to_owned(), Value::String(notes.join("; ")));
        document.extend(metrics);
        Value::Object(document)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Best-effort thermal metrics from hwmon.
///
/// Returns `(cpu_temp_c, gpu_temp_c, max_fan_rpm)`. A missing hwmon tree is
/// normal on VMs/servers and yields `None` values rather than fake zeroes.
fn thermal_metrics(root: &Path) -> (Option<f64>, Option<f64>, Option<u32>) {
    let Ok(inventory) = HwmonInventory::discover(root) else {
        return (None, None, None);
    };
    let sensors = inventory.read_sensors().unwrap_or_default();
    let fans = inventory.read_fans().unwrap_or_default();

    let cpu = max_matching_temp(
        &sensors,
        &[
            "cpu", "core", "package", "tdie", "tctl", "k10temp", "coretemp",
        ],
    );
    let gpu = max_matching_temp(&sensors, &["gpu", "amdgpu", "nvidia", "nouveau", "radeon"]);
    let fan_rpm = fans.iter().filter_map(|fan| fan.rpm).max();
    (cpu, gpu, fan_rpm)
}

fn max_matching_temp(sensors: &[ozma_thermal::SensorReading], needles: &[&str]) -> Option<f64> {
    sensors
        .iter()
        .filter(|sensor| {
            let id = sensor.id.to_ascii_lowercase();
            let name = sensor.name.to_ascii_lowercase();
            needles
                .iter()
                .any(|needle| id.contains(needle) || name.contains(needle))
        })
        .map(|sensor| sensor.temp_c)
        .filter(|value| value.is_finite())
        .max_by(|left, right| left.total_cmp(right))
}

fn read_bounded(path: &Path) -> std::io::Result<String> {
    let file = File::open(path)?;
    let mut reader = file.take(MAX_PROC_BYTES);
    let mut buffer = Vec::new();
    reader.read_to_end(&mut buffer)?;
    String::from_utf8(buffer)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn parse_cpu_totals(text: &str) -> Option<CpuTotals> {
    let line = text.lines().find(|line| line.starts_with("cpu "))?;
    let mut fields = line.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    // guest/guest_nice are already folded into user/nice; summing all ten
    // fields would count them twice. Eight fields (user..steal) are enough.
    let values: Option<Vec<u64>> = fields.take(8).map(|field| field.parse().ok()).collect();
    let values = values?;
    if values.len() < 4 {
        return None;
    }
    let total = values.iter().copied().sum();
    let idle = values.get(3).copied().unwrap_or(0) + values.get(4).copied().unwrap_or(0);
    Some(CpuTotals { total, idle })
}

fn cpu_percent(previous: CpuTotals, current: CpuTotals) -> Option<f64> {
    let total = current.total.checked_sub(previous.total)?;
    let idle = current.idle.checked_sub(previous.idle)?;
    if total == 0 || idle > total {
        return None;
    }
    Some((total - idle) as f64 * 100.0 / total as f64)
}

fn cpu_percent_since_boot(current: CpuTotals) -> Option<f64> {
    if current.total == 0 || current.idle > current.total {
        return None;
    }
    Some((current.total - current.idle) as f64 * 100.0 / current.total as f64)
}

fn parse_memory(text: &str) -> Option<MemoryTotals> {
    let mut total_kb = None;
    let mut available_kb = None;
    let mut free_kb = 0u64;
    let mut buffers_kb = 0u64;
    let mut cached_kb = 0u64;
    let mut saw_free = false;

    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(value) = rest
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };
        match name.trim() {
            "MemTotal" => total_kb = Some(value),
            "MemAvailable" => available_kb = Some(value),
            "MemFree" => {
                free_kb = value;
                saw_free = true;
            }
            "Buffers" => buffers_kb = value,
            "Cached" => cached_kb = value,
            _ => {}
        }
    }

    let total_kb = total_kb?;
    let available_kb = available_kb.or_else(|| {
        saw_free.then(|| free_kb.saturating_add(buffers_kb).saturating_add(cached_kb))
    })?;
    let total_bytes = total_kb.saturating_mul(1024);
    let available_bytes = available_kb.saturating_mul(1024).min(total_bytes);
    Some(MemoryTotals {
        total_bytes,
        used_bytes: total_bytes.saturating_sub(available_bytes),
    })
}

fn parse_net_totals(text: &str) -> Option<NetTotals> {
    let mut rx_bytes = 0u64;
    let mut tx_bytes = 0u64;
    let mut seen = false;

    for line in text.lines() {
        let Some((interface, rest)) = line.split_once(':') else {
            continue;
        };
        // Loopback traffic is real but would dominate a host dashboard and is
        // not what an operator means by network throughput.
        if interface.trim() == "lo" {
            continue;
        }
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() < 9 {
            continue;
        }
        let Some(rx) = fields[0].parse::<u64>().ok() else {
            continue;
        };
        let Some(tx) = fields[8].parse::<u64>().ok() else {
            continue;
        };
        rx_bytes = rx_bytes.saturating_add(rx);
        tx_bytes = tx_bytes.saturating_add(tx);
        seen = true;
    }

    seen.then_some(NetTotals { rx_bytes, tx_bytes })
}

fn net_rates(previous: NetSample, current: NetTotals, now: Instant) -> Option<(f64, f64)> {
    let elapsed = now.duration_since(previous.at).as_secs_f64();
    if elapsed < MIN_RATE_INTERVAL.as_secs_f64() {
        return None;
    }
    if current.rx_bytes < previous.totals.rx_bytes || current.tx_bytes < previous.totals.tx_bytes {
        // Counter reset/rollover: a rate would be invented.
        return None;
    }
    let rx = (current.rx_bytes - previous.totals.rx_bytes) as f64 / elapsed;
    let tx = (current.tx_bytes - previous.totals.tx_bytes) as f64 / elapsed;
    Some((rx, tx))
}

fn parse_uptime(text: &str) -> Option<u64> {
    let value = text.split_whitespace().next()?.parse::<f64>().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value as u64)
}

#[cfg(unix)]
fn disk_usage(path: &Path) -> Option<(u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a valid NUL-terminated C string and `stats` points to
    // writable memory of the correct type.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    let block_size = if stats.f_frsize == 0 {
        stats.f_bsize
    } else {
        stats.f_frsize
    }
    .max(1);
    let total = stats.f_blocks.saturating_mul(block_size);
    let free = stats.f_bavail.saturating_mul(block_size).min(total);
    Some((total, total.saturating_sub(free)))
}

#[cfg(not(unix))]
fn disk_usage(_path: &Path) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn number(value: f64) -> Value {
    Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const PROC_STAT: &str = "cpu  100 0 50 850 10 0 0 0 0 0\ncpu0 1 2 3 4 5 6 7 8 9 10\n";
    const PROC_MEMINFO: &str = "MemTotal:       1000000 kB\nMemFree:         200000 kB\nMemAvailable:    400000 kB\nBuffers:          10000 kB\nCached:          100000 kB\n";
    const PROC_NETDEV: &str = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    lo: 999999 0 0 0 0 0 0 0 999999 0 0 0 0 0 0 0\n  eth0: 1000 1 0 0 0 0 0 0 2000 2 0 0 0 0 0 0\n";
    const PROC_UPTIME: &str = "1234.56 900.00\n";

    fn fake_proc_tree(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "ozma-agent-metrics-{label}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let proc_root = root.join("proc");
        fs::create_dir_all(&proc_root).unwrap();
        fs::write(proc_root.join("stat"), PROC_STAT).unwrap();
        fs::write(proc_root.join("meminfo"), PROC_MEMINFO).unwrap();
        fs::create_dir_all(proc_root.join("net")).unwrap();
        fs::write(proc_root.join("net").join("dev"), PROC_NETDEV).unwrap();
        fs::write(proc_root.join("uptime"), PROC_UPTIME).unwrap();
        root
    }

    #[test]
    fn parses_cpu_memory_and_network_fixtures() {
        let cpu = parse_cpu_totals(PROC_STAT).expect("cpu fixture");
        assert_eq!(cpu.total, 1010);
        assert_eq!(cpu.idle, 860);
        assert!((cpu_percent_since_boot(cpu).unwrap() - 14.851_485).abs() < 0.01);

        let memory = parse_memory(PROC_MEMINFO).expect("memory fixture");
        assert_eq!(memory.total_bytes, 1_024_000_000);
        assert_eq!(memory.used_bytes, 614_400_000);

        let net = parse_net_totals(PROC_NETDEV).expect("net fixture");
        assert_eq!(
            net,
            NetTotals {
                rx_bytes: 1000,
                tx_bytes: 2000
            },
            "loopback must be excluded"
        );

        assert_eq!(parse_uptime(PROC_UPTIME), Some(1234));
    }

    #[test]
    fn memory_falls_back_to_free_plus_buffers_plus_cached() {
        let memory =
            parse_memory("MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 50 kB\nCached: 150 kB\n")
                .expect("fallback memory fixture");
        assert_eq!(memory.total_bytes, 1_024_000);
        assert_eq!(memory.used_bytes, 716_800);
    }

    #[test]
    fn collects_a_flat_honest_document() {
        let root = fake_proc_tree("collect");
        let paths = MetricsPaths {
            proc_root: root.join("proc"),
            disk_path: root.clone(),
            hwmon_root: root.join("hwmon"),
        };
        let sampler = AgentMetricsSampler::default();

        let first = sampler.collect_with(&paths, "metrics-node._ozma._udp.local.");
        assert_eq!(first["ok"], true);
        assert_eq!(first["available"], true);
        assert_eq!(first["source"], "ozma-node");
        assert_eq!(first["scope"], "host");
        assert_eq!(first["node_id"], "metrics-node._ozma._udp.local.");
        assert!(first["cpu_usage"].as_f64().unwrap() > 0.0);
        assert_eq!(first["ram_total"], 1_024_000_000u64);
        assert_eq!(first["ram_used"], 614_400_000u64);
        assert!(first["disk_total"].as_u64().unwrap() > 0);
        assert!(first["disk_used"].as_u64().unwrap() <= first["disk_total"].as_u64().unwrap());
        assert_eq!(first["uptime"], 1234);
        assert!(first["net_rx_rate"].is_null());
        assert!(first["unavailable"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "net_rx_rate"));

        std::thread::sleep(Duration::from_millis(60));
        fs::write(
            paths.proc_root.join("net/dev"),
            PROC_NETDEV.replace(
                "eth0: 1000 1 0 0 0 0 0 0 2000 2",
                "eth0: 11000 1 0 0 0 0 0 0 22000 2",
            ),
        )
        .unwrap();

        let second = sampler.collect_with(&paths, "metrics-node._ozma._udp.local.");
        assert!(second["net_rx_rate"].as_f64().unwrap() > 0.0);
        assert!(second["net_tx_rate"].as_f64().unwrap() > 0.0);
        assert!(!second["unavailable"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "net_rx_rate"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn thermal_metrics_come_from_hwmon_when_available() {
        let root = std::env::temp_dir().join(format!(
            "ozma-agent-metrics-thermal-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let coretemp = root.join("hwmon/hwmon0");
        let amdgpu = root.join("hwmon/hwmon1");
        fs::create_dir_all(&coretemp).unwrap();
        fs::create_dir_all(&amdgpu).unwrap();
        fs::write(coretemp.join("name"), "coretemp\n").unwrap();
        fs::write(coretemp.join("temp1_input"), "55000\n").unwrap();
        fs::write(coretemp.join("fan1_input"), "1200\n").unwrap();
        fs::write(amdgpu.join("name"), "amdgpu\n").unwrap();
        fs::write(amdgpu.join("temp1_input"), "70000\n").unwrap();

        let paths = MetricsPaths {
            proc_root: root.join("proc-does-not-exist"),
            disk_path: root.join("disk-does-not-exist"),
            hwmon_root: root.join("hwmon"),
        };
        let document = AgentMetricsSampler::default().collect_with(&paths, "thermal-node");

        assert_eq!(document["cpu_temp"], 55.0);
        assert_eq!(document["gpu_temp"], 70.0);
        assert_eq!(document["fan_rpm"], 1200);
        let unavailable = document["unavailable"].as_array().unwrap();
        for key in ["cpu_temp", "gpu_temp", "fan_rpm"] {
            assert!(
                !unavailable.iter().any(|value| value == key),
                "{key} must not be unavailable"
            );
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn labels_missing_sources_instead_of_reporting_zero() {
        let root = std::env::temp_dir().join(format!(
            "ozma-agent-metrics-missing-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let paths = MetricsPaths {
            proc_root: root.join("proc-does-not-exist"),
            disk_path: root.join("disk-does-not-exist"),
            hwmon_root: root.join("hwmon-does-not-exist"),
        };
        let document = AgentMetricsSampler::default().collect_with(&paths, "missing-node");

        assert_eq!(document["ok"], true);
        assert_eq!(document["available"], false);
        for key in [
            "cpu_usage",
            "ram_used",
            "disk_total",
            "net_rx_rate",
            "uptime",
        ] {
            assert!(document.get(key).is_none(), "{key} must be omitted");
            assert!(
                document["unavailable"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|value| value == key),
                "{key} must be labelled unavailable"
            );
        }
    }
}
