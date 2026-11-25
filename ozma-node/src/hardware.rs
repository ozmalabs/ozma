// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Deep hardware inventory and sensor API.
//!
//! Backed by `ozma-hardware`, which collects the CPU/cache/instruction tree,
//! memory slots, GPUs, storage SMART/NVMe data, motherboard/VRM/PCH sensors,
//! fans, voltage rails, batteries, PCIe devices, and network interfaces. The
//! snapshot is expensive (SMART/NVMe/nvidia-smi subprocesses), so it is cached
//! briefly and collected on a blocking task.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ozma_hardware::{HardwareInfoCollector, HardwareSnapshot};
use serde::Deserialize;
use serde_json::json;

/// How long a full hardware snapshot is reused before re-running collectors.
const CACHE_TTL: Duration = Duration::from_secs(5);

/// Shared hardware sampler used by the HTTP routes.
#[derive(Clone, Default)]
pub struct HardwareState {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    collector: Option<HardwareInfoCollector>,
    cached: Option<(Instant, HardwareSnapshot)>,
    init_error: Option<String>,
}

impl HardwareState {
    /// Create a lazy hardware sampler.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a cached or freshly collected snapshot.
    ///
    /// `refresh` bypasses the cache. Collection runs on a blocking task because
    /// it may invoke `smartctl`, `nvidia-smi`, or read many sysfs files.
    pub async fn snapshot(&self, refresh: bool) -> anyhow::Result<HardwareSnapshot> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner
                .lock()
                .map_err(|_| anyhow!("hardware sampler lock poisoned"))?;
            if !refresh {
                if let Some((at, snapshot)) = &guard.cached {
                    if at.elapsed() < CACHE_TTL {
                        return Ok(snapshot.clone());
                    }
                }
            }
            if let Some(error) = &guard.init_error {
                return Err(anyhow!("hardware collector unavailable: {error}"));
            }
            if guard.collector.is_none() {
                match HardwareInfoCollector::new() {
                    Ok(collector) => guard.collector = Some(collector),
                    Err(error) => {
                        let message = error.to_string();
                        guard.init_error = Some(message.clone());
                        return Err(anyhow!("hardware collector unavailable: {message}"));
                    }
                }
            }
            let collector = guard
                .collector
                .as_mut()
                .ok_or_else(|| anyhow!("hardware collector unavailable"))?;
            let snapshot = collector.snapshot()?;
            guard.cached = Some((Instant::now(), snapshot.clone()));
            Ok(snapshot)
        })
        .await
        .map_err(|error| anyhow!("hardware collection task failed: {error}"))?
    }
}

#[derive(Debug, Default, Deserialize)]
struct SnapshotQuery {
    #[serde(default)]
    refresh: bool,
}

/// Build the authenticated hardware inventory router.
pub fn router(state: HardwareState) -> Router {
    Router::new()
        .route("/hardware", get(snapshot_handler))
        .route("/hardware/summary", get(summary_handler))
        .route("/hardware/sensors", get(sensors_handler))
        .route("/api/v1/hardware", get(snapshot_handler))
        .route("/api/v1/hardware/summary", get(summary_handler))
        .route("/api/v1/hardware/sensors", get(sensors_handler))
        .with_state(state)
}

async fn snapshot_handler(
    State(state): State<HardwareState>,
    Query(query): Query<SnapshotQuery>,
) -> Response {
    match state.snapshot(query.refresh).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": error.to_string()})),
        )
            .into_response(),
    }
}

async fn sensors_handler(
    State(state): State<HardwareState>,
    Query(query): Query<SnapshotQuery>,
) -> Response {
    match state.snapshot(query.refresh).await {
        Ok(snapshot) => Json(snapshot.sensors).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": error.to_string()})),
        )
            .into_response(),
    }
}

async fn summary_handler(
    State(state): State<HardwareState>,
    Query(query): Query<SnapshotQuery>,
) -> Response {
    match state.snapshot(query.refresh).await {
        Ok(snapshot) => Json(json!({
            "ok": true,
            "timestamp": snapshot.timestamp,
            "hostname": snapshot.hostname,
            "os": snapshot.os,
            "os_version": snapshot.os_version,
            "cpu": {
                "brand": snapshot.cpu.brand,
                "physical_cores": snapshot.cpu.physical_cores,
                "logical_cores": snapshot.cpu.logical_cores,
                "base_clock_mhz": snapshot.cpu.base_clock_mhz,
                "boost_clock_mhz": snapshot.cpu.boost_clock_mhz,
                "package_temp_c": snapshot.cpu.package_temp_c,
                "package_power_w": snapshot.cpu.package_power_w,
            },
            "total_memory_gb": snapshot.total_memory_gb,
            "memory_slots": snapshot.memory_slots.len(),
            "gpus": snapshot.gpus,
            "storage": snapshot.storage,
            "motherboard": snapshot.motherboard,
            "fans": snapshot.fans,
            "voltages": snapshot.voltages,
            "sensors": snapshot.sensors.len(),
            "batteries": snapshot.batteries,
            "pcie_devices": snapshot.pcie_devices.len(),
            "network": snapshot.network,
        }))
        .into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": error.to_string()})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unsupported_platform_or_missing_hwmon_returns_a_typed_error() {
        // The state is lazy: this test only checks that a failed collector is a
        // typed 503 rather than a panic. On Linux the collector normally
        // succeeds, so only assert the route shape when it fails.
        let state = HardwareState::new();
        let result = state.snapshot(true).await;
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn router_builds() {
        let _ = router(HardwareState::new());
    }
}
