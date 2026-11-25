// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! AIDA64-compatible sensor feed and Ozma widget panel.
//!
//! AIDA64's RemoteSensor exposes HTTP/SSE at `/sse`, JSON/JSONP at `/api`, and
//! an HTML panel at `/`. The sensor value IDs (`TCPU`, `SCPUUTI`, `FGPU1`,
//! `SMEMUTI`, ...) are the documented AIDA64 IDs, so existing AIDA64 viewers,
//! ESP32 panels, and RemoteSensor clients work without modification.
//!
//! The same snapshot also produces an Ozma `ScreenLayout` using the existing
//! gauge/bar/number widget schema. That layout can be rendered by the browser
//! display adapter or the controller's native screen renderer.
//!
//! This listener is opt-in (`--aida64-port`) and unauthenticated, because
//! AIDA64 RemoteSensor clients do not send an Authorization header.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt::Write as _;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::header;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::stream::{self, Stream};
use ozma_display_ir::{
    canonical_json, widget_irs_to_screen_layout, DataBinding, LayoutHint, SourceDocument, StyleRef,
    Threshold, TypographyRef, ValueDomain, WidgetIR,
};
use ozma_hardware::{HardwareSnapshot, StorageInfo};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use sysinfo::{CpuExt, System, SystemExt};
use tokio_util::sync::CancellationToken;

use crate::hardware::HardwareState;

/// One AIDA64 sensor value.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum AidaValue {
    Number(f64),
    Text(String),
}

impl AidaValue {
    fn number(&self) -> Option<f64> {
        match self {
            Self::Number(value) => Some(*value),
            Self::Text(_) => None,
        }
    }

    fn display(&self) -> String {
        match self {
            Self::Number(value) => format_number(*value),
            Self::Text(value) => value.clone(),
        }
    }
}

/// One sensor in AIDA64 vocabulary.
#[derive(Clone, Debug, Serialize)]
pub struct AidaSensor {
    pub id: String,
    pub label: String,
    pub value: AidaValue,
    pub unit: String,
    pub kind: String,
    pub color: String,
}

/// Complete AIDA64-compatible snapshot.
#[derive(Clone, Debug, Default, Serialize)]
pub struct AidaSnapshot {
    pub timestamp: f64,
    pub sensors: Vec<AidaSensor>,
}

impl AidaSnapshot {
    /// Return only numeric sensors, suitable for `DataUpdate`/metric ingestion.
    pub fn numeric_metrics(&self) -> BTreeMap<String, f64> {
        self.sensors
            .iter()
            .filter_map(|sensor| Some((sensor.id.clone(), sensor.value.number()?)))
            .collect()
    }

    /// Flat field map for a widget panel.
    pub fn panel_values(&self) -> Value {
        let mut map = Map::new();
        for sensor in &self.sensors {
            map.insert(
                format!("@active.{}", sensor.id),
                match &sensor.value {
                    AidaValue::Number(value) => json!(value),
                    AidaValue::Text(value) => json!(value),
                },
            );
        }
        Value::Object(map)
    }

    fn by_id(&self, id: &str) -> Option<&AidaSensor> {
        self.sensors.iter().find(|sensor| sensor.id == id)
    }
}

/// System-wide values AIDA exposes beyond the static hardware tree.
#[derive(Clone, Debug, Default)]
pub struct SystemUsage {
    pub cpu_percent: f64,
    pub per_core_percent: Vec<f64>,
    pub used_mib: f64,
    pub total_mib: f64,
    pub uptime_secs: u64,
    pub processes: usize,
}

fn collect_system_usage() -> SystemUsage {
    let mut system = System::new_all();
    system.refresh_cpu();
    system.refresh_memory();
    SystemUsage {
        cpu_percent: system.global_cpu_info().cpu_usage() as f64,
        per_core_percent: system
            .cpus()
            .iter()
            .map(|cpu| cpu.cpu_usage() as f64)
            .collect(),
        used_mib: system.used_memory() as f64 / (1024.0 * 1024.0),
        total_mib: system.total_memory() as f64 / (1024.0 * 1024.0),
        uptime_secs: system.uptime(),
        processes: system.processes().len(),
    }
}

/// Build an AIDA64 snapshot from a hardware snapshot.
pub fn build_snapshot(hardware: &HardwareSnapshot) -> AidaSnapshot {
    build_snapshot_with_usage(hardware, &collect_system_usage())
}

/// Build an AIDA64 snapshot with explicit system usage (testable).
pub fn build_snapshot_with_usage(hardware: &HardwareSnapshot, usage: &SystemUsage) -> AidaSnapshot {
    let mut sensors: BTreeMap<String, AidaSensor> = BTreeMap::new();
    let now = hardware.timestamp.max(0.0);
    let (date, time, short_time) = utc_parts(now);
    let (hours, minutes, seconds) = uptime_parts(usage.uptime_secs);

    push_text(&mut sensors, "SDATE", "Date", date, "system");
    push_text(&mut sensors, "STIME", "Time", time, "system");
    push_text(
        &mut sensors,
        "STIMENS",
        "Time (HH:MM)",
        short_time,
        "system",
    );
    push_text(
        &mut sensors,
        "SUPTIME",
        "UpTime",
        format!("{hours:02}:{minutes:02}:{seconds:02}"),
        "system",
    );
    push_text(
        &mut sensors,
        "SUPTIMENS",
        "UpTime (HH:MM)",
        format!("{hours:02}:{minutes:02}"),
        "system",
    );
    push_number(
        &mut sensors,
        "SCPUUTI",
        "CPU Utilization",
        usage.cpu_percent,
        "%",
        "load",
    );
    for (index, value) in usage.per_core_percent.iter().enumerate() {
        push_number(
            &mut sensors,
            &format!("SCPU{}UTI", index + 1),
            &format!("CPU{} Utilization", index + 1),
            *value,
            "%",
            "load",
        );
    }
    push_number(
        &mut sensors,
        "SMEMUTI",
        "Memory Utilization",
        if usage.total_mib > 0.0 {
            usage.used_mib / usage.total_mib * 100.0
        } else {
            0.0
        },
        "%",
        "load",
    );
    push_number(
        &mut sensors,
        "SUSEDMEM",
        "Used Memory",
        usage.used_mib,
        "MB",
        "system",
    );
    push_number(
        &mut sensors,
        "SFREEMEM",
        "Free Memory",
        (usage.total_mib - usage.used_mib).max(0.0),
        "MB",
        "system",
    );
    push_number(
        &mut sensors,
        "SPROCESSES",
        "Processes",
        usage.processes as f64,
        "",
        "system",
    );
    push_text(
        &mut sensors,
        "SMOBONAME",
        "Motherboard Name",
        if hardware.motherboard.model.is_empty() {
            hardware.motherboard.manufacturer.clone()
        } else {
            hardware.motherboard.model.clone()
        },
        "system",
    );
    push_text(
        &mut sensors,
        "SBIOSVER",
        "BIOS Version",
        hardware.motherboard.bios_version.clone(),
        "system",
    );

    let cpu = &hardware.cpu;
    let cpu_temp = if cpu.package_temp_c > 0.0 {
        cpu.package_temp_c
    } else {
        cpu.cores
            .iter()
            .map(|core| core.temperature_c)
            .fold(0.0, f32::max)
    };
    if cpu_temp > 0.0 {
        push_number(
            &mut sensors,
            "TCPU",
            "CPU",
            cpu_temp as f64,
            "C",
            "temperature",
        );
        push_number(
            &mut sensors,
            "TCPUPKG",
            "CPU Package",
            cpu_temp as f64,
            "C",
            "temperature",
        );
    }
    for (index, core) in cpu.cores.iter().enumerate() {
        if core.temperature_c > 0.0 {
            push_number(
                &mut sensors,
                &format!("TCC-1-{:02}", index + 1),
                &format!("CC-1-{} (CPU #1 / Core #{})", index + 1, index + 1),
                core.temperature_c as f64,
                "C",
                "temperature",
            );
        }
        if core.clock_mhz > 0.0 {
            push_number(
                &mut sensors,
                &format!("SCC-1-{:02}", index + 1),
                &format!("CC-1-{} Clock", index + 1),
                core.clock_mhz as f64,
                "MHz",
                "clock",
            );
        }
    }
    if cpu.core_voltage_v > 0.0 {
        push_number(
            &mut sensors,
            "VCPU",
            "CPU Core",
            cpu.core_voltage_v as f64,
            "V",
            "voltage",
        );
    }
    if cpu.package_power_w > 0.0 {
        push_number(
            &mut sensors,
            "PCPUPKG",
            "CPU Package",
            cpu.package_power_w as f64,
            "W",
            "power",
        );
    }
    if cpu.boost_clock_mhz > 0.0 {
        push_number(
            &mut sensors,
            "SCPUMUL",
            "CPU Multiplier",
            (cpu.boost_clock_mhz / 100.0) as f64,
            "",
            "clock",
        );
    }
    push_number(
        &mut sensors,
        "SCPUCLK",
        "CPU Clock",
        cpu.cores
            .iter()
            .map(|core| core.clock_mhz)
            .fold(cpu.base_clock_mhz, f32::max) as f64,
        "MHz",
        "clock",
    );

    if let Some(slot) = hardware.memory_slots.first() {
        push_text(
            &mut sensors,
            "SMEMSPEED",
            "Memory Speed",
            if slot.memory_type.is_empty() {
                format!("{} MHz", slot.speed_mhz)
            } else {
                format!("{} {}", slot.memory_type, slot.speed_mhz)
            },
            "system",
        );
        if slot.configured_speed_mhz > 0 {
            push_number(
                &mut sensors,
                "SMEMCLK",
                "Memory Clock",
                slot.configured_speed_mhz as f64 / 2.0,
                "MHz",
                "clock",
            );
        }
        if slot.timing.cl > 0 {
            push_text(
                &mut sensors,
                "SMEMTIM",
                "Memory Timings",
                format!(
                    "{}-{}-{}-{}",
                    slot.timing.cl, slot.timing.trcd, slot.timing.trp, slot.timing.tras
                ),
                "system",
            );
        }
    }

    for gpu in &hardware.gpus {
        let index = gpu.index + 1;
        if gpu.temperature_c > 0.0 {
            push_number(
                &mut sensors,
                &format!("TGPU{index}"),
                &format!("GPU{index}"),
                gpu.temperature_c as f64,
                "C",
                "temperature",
            );
        }
        if gpu.hotspot_temp_c > 0.0 {
            push_number(
                &mut sensors,
                &format!("TGPU{index}HOT"),
                &format!("GPU{index} Hotspot"),
                gpu.hotspot_temp_c as f64,
                "C",
                "temperature",
            );
        }
        if gpu.vram_temp_c > 0.0 {
            push_number(
                &mut sensors,
                &format!("TGPU{index}MEM"),
                &format!("GPU{index} Memory"),
                gpu.vram_temp_c as f64,
                "C",
                "temperature",
            );
        }
        if gpu.utilization_percent > 0.0 {
            push_number(
                &mut sensors,
                &format!("SGPU{index}UTI"),
                &format!("GPU{index} Utilization"),
                gpu.utilization_percent as f64,
                "%",
                "load",
            );
        }
        if gpu.core_clock_mhz > 0.0 {
            push_number(
                &mut sensors,
                &format!("SGPU{index}CLK"),
                &format!("GPU{index} Clock"),
                gpu.core_clock_mhz as f64,
                "MHz",
                "clock",
            );
        }
        if gpu.memory_clock_mhz > 0.0 {
            push_number(
                &mut sensors,
                &format!("SGPU{index}MEMCLK"),
                &format!("GPU{index} Memory Clock"),
                gpu.memory_clock_mhz as f64,
                "MHz",
                "clock",
            );
        }
        if gpu.power_w > 0.0 {
            push_number(
                &mut sensors,
                &format!("PGPU{index}"),
                &format!("GPU{index}"),
                gpu.power_w as f64,
                "W",
                "power",
            );
        }
        if gpu.fan_rpm > 0 {
            push_number(
                &mut sensors,
                &format!("FGPU{index}"),
                &format!("GPU{index}"),
                gpu.fan_rpm as f64,
                "RPM",
                "fan",
            );
        }
    }

    if hardware.motherboard.ambient_temp_c > 0.0 {
        push_number(
            &mut sensors,
            "TMOBO",
            "Motherboard",
            hardware.motherboard.ambient_temp_c as f64,
            "C",
            "temperature",
        );
    }
    if hardware.motherboard.vrm_temp_c > 0.0 {
        push_number(
            &mut sensors,
            "TVRM",
            "VRM",
            hardware.motherboard.vrm_temp_c as f64,
            "C",
            "temperature",
        );
    }
    if hardware.motherboard.pch_temp_c > 0.0 {
        push_number(
            &mut sensors,
            "TPCH",
            "PCH",
            hardware.motherboard.pch_temp_c as f64,
            "C",
            "temperature",
        );
    }

    let mut fan_index = 0;
    for fan in &hardware.fans {
        if fan.rpm == 0 {
            continue;
        }
        let lower = fan.name.to_ascii_lowercase();
        let id = if lower.contains("cpu") {
            "FCPU".to_string()
        } else {
            fan_index += 1;
            format!("FCHA{fan_index}")
        };
        push_number(&mut sensors, &id, &fan.name, fan.rpm as f64, "RPM", "fan");
    }

    let mut rail_index = 0;
    for rail in &hardware.voltages {
        if rail.voltage_v <= 0.0 {
            continue;
        }
        let lower = rail.name.to_ascii_lowercase();
        let id = if lower.contains("12v") {
            "VP12V".to_string()
        } else if lower.contains("5v") {
            "VP5V".to_string()
        } else if lower.contains("3.3") || lower.contains("3v3") {
            "V33V".to_string()
        } else if lower.contains("soc") {
            "VSOC".to_string()
        } else if lower.contains("vddio") {
            "VVDDIO".to_string()
        } else {
            rail_index += 1;
            format!("VRAIL{rail_index}")
        };
        push_number(
            &mut sensors,
            &id,
            &rail.name,
            rail.voltage_v as f64,
            "V",
            "voltage",
        );
    }

    for (index, disk) in hardware.storage.iter().enumerate() {
        let number = index + 1;
        if disk.temperature_c > 0.0 {
            push_number(
                &mut sensors,
                &format!("THDD{number}"),
                storage_label(disk),
                disk.temperature_c as f64,
                "C",
                "temperature",
            );
        }
        if disk.read_rate_mbs > 0.0 || disk.write_rate_mbs > 0.0 {
            push_number(
                &mut sensors,
                &format!("SDSK{number}ACT"),
                &format!("Disk {number} Activity"),
                (disk.read_rate_mbs + disk.write_rate_mbs) as f64,
                "MB/s",
                "load",
            );
            push_number(
                &mut sensors,
                &format!("SDSK{number}READSPD"),
                &format!("Disk {number} Read Speed"),
                disk.read_rate_mbs as f64,
                "MB/s",
                "load",
            );
            push_number(
                &mut sensors,
                &format!("SDSK{number}WRITESPD"),
                &format!("Disk {number} Write Speed"),
                disk.write_rate_mbs as f64,
                "MB/s",
                "load",
            );
        }
    }

    for (index, nic) in hardware.network.iter().enumerate() {
        let number = index + 1;
        push_number(
            &mut sensors,
            &format!("SNIC{number}DL"),
            &format!("NIC{number} Download Rate"),
            nic.rx_rate_mbs as f64,
            "MB/s",
            "load",
        );
        push_number(
            &mut sensors,
            &format!("SNIC{number}UL"),
            &format!("NIC{number} Upload Rate"),
            nic.tx_rate_mbs as f64,
            "MB/s",
            "load",
        );
        push_number(
            &mut sensors,
            &format!("SNIC{number}TOTDL"),
            &format!("NIC{number} Total Download"),
            nic.rx_bytes as f64 / (1024.0 * 1024.0),
            "MB",
            "system",
        );
        push_number(
            &mut sensors,
            &format!("SNIC{number}TOTUL"),
            &format!("NIC{number} Total Upload"),
            nic.tx_bytes as f64 / (1024.0 * 1024.0),
            "MB",
            "system",
        );
    }

    if let Some(battery) = hardware.batteries.first() {
        let percent = battery.charge_percent();
        if percent > 0.0 {
            push_number(
                &mut sensors,
                "SBATT",
                "Battery",
                percent as f64,
                "%",
                "system",
            );
        }
        push_text(
            &mut sensors,
            "SBATTSTATUS",
            "Battery Status",
            format!("{} {}", battery.status, battery.technology),
            "system",
        );
    }

    AidaSnapshot {
        timestamp: now,
        sensors: sensors.into_values().collect(),
    }
}

fn storage_label(disk: &StorageInfo) -> &str {
    if !disk.model.is_empty() {
        &disk.model
    } else if !disk.device.is_empty() {
        &disk.device
    } else {
        "Disk"
    }
}

fn push_number(
    sensors: &mut BTreeMap<String, AidaSensor>,
    id: &str,
    label: &str,
    value: f64,
    unit: &str,
    kind: &str,
) {
    if !value.is_finite() {
        return;
    }
    sensors.entry(id.to_string()).or_insert(AidaSensor {
        id: id.to_string(),
        label: label.to_string(),
        value: AidaValue::Number(value),
        unit: unit.to_string(),
        kind: kind.to_string(),
        color: color_for(kind),
    });
}

fn push_text(
    sensors: &mut BTreeMap<String, AidaSensor>,
    id: &str,
    label: &str,
    value: String,
    kind: &str,
) {
    if value.is_empty() {
        return;
    }
    sensors.entry(id.to_string()).or_insert(AidaSensor {
        id: id.to_string(),
        label: label.to_string(),
        value: AidaValue::Text(value),
        unit: String::new(),
        kind: kind.to_string(),
        color: color_for(kind),
    });
}

fn color_for(kind: &str) -> String {
    match kind {
        "temperature" => "#f0b94a",
        "fan" => "#38bdf8",
        "voltage" => "#a78bfa",
        "power" => "#f26464",
        "load" => "#3ecf8e",
        "clock" => "#5b6fff",
        _ => "#9aa4b2",
    }
    .to_string()
}

fn format_number(value: f64) -> String {
    if !value.is_finite() {
        return "-".to_string();
    }
    if (value - value.round()).abs() < 0.05 {
        format!("{value:.0}")
    } else if value.abs() < 10.0 {
        format!("{value:.2}")
    } else {
        format!("{value:.1}")
    }
}

fn utc_parts(timestamp: f64) -> (String, String, String) {
    let secs = timestamp.max(0.0).floor() as i64;
    let days = secs.div_euclid(86_400);
    let second_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3600;
    let minute = (second_of_day % 3600) / 60;
    let second = second_of_day % 60;
    (
        format!("{day:02}.{month:02}.{year:04}"),
        format!("{hour:02}:{minute:02}:{second:02}"),
        format!("{hour:02}:{minute:02}"),
    )
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + i64::from(m <= 2), m, d)
}

fn uptime_parts(seconds: u64) -> (u64, u64, u64) {
    (seconds / 3600, (seconds % 3600) / 60, seconds % 60)
}

/// AIDA64 SSE payload: `Page0|{|}Simple1|value{|}Simple2|value...`
pub fn encode_sse(snapshot: &AidaSnapshot) -> String {
    let mut out = String::from("Page0|{|}");
    for (index, sensor) in snapshot.sensors.iter().enumerate() {
        let value = sanitize_sse(&format!("{}{}", sensor.value.display(), sensor.unit));
        let _ = write!(out, "Simple{}|{value}{{|}}", index + 1);
    }
    out
}

fn sanitize_sse(value: &str) -> String {
    value.replace("{|}", " ").replace('|', " ")
}

fn valid_callback(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '$')
        })
}

#[derive(Debug, Deserialize)]
struct ApiQuery {
    callback: Option<String>,
    sensors: Option<String>,
}

/// Build the AIDA64-compatible router.
pub fn router(hardware: HardwareState) -> Router {
    Router::new()
        .route("/", get(panel_html))
        .route("/sse", get(sse_handler))
        .route("/api", get(api_handler))
        .route("/sensors", get(sensors_handler))
        .route("/panel", get(panel_layout_handler))
        .route("/panel/data", get(panel_data_handler))
        .with_state(AidaState { hardware })
}

#[derive(Clone)]
struct AidaState {
    hardware: HardwareState,
}

async fn snapshot_or_empty(state: &AidaState) -> AidaSnapshot {
    match state.hardware.snapshot(false).await {
        Ok(hardware) => build_snapshot(&hardware),
        Err(error) => {
            tracing::warn!(error = %error, "AIDA64 hardware snapshot unavailable");
            AidaSnapshot::default()
        }
    }
}

async fn sensors_handler(State(state): State<AidaState>) -> Json<AidaSnapshot> {
    Json(snapshot_or_empty(&state).await)
}

async fn panel_data_handler(State(state): State<AidaState>) -> Json<Value> {
    Json(snapshot_or_empty(&state).await.panel_values())
}

async fn panel_layout_handler(State(state): State<AidaState>) -> Json<Value> {
    Json(panel_layout(&snapshot_or_empty(&state).await))
}

async fn api_handler(State(state): State<AidaState>, Query(query): Query<ApiQuery>) -> Response {
    let snapshot = snapshot_or_empty(&state).await;
    let selected = query.sensors.as_deref().map(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    });
    let mut map = Map::new();
    for sensor in &snapshot.sensors {
        if let Some(selected) = &selected {
            if !selected.iter().any(|id| id == &sensor.id) {
                continue;
            }
        }
        map.insert(
            sensor.id.clone(),
            json!({
                "label": sensor.label,
                "value": match &sensor.value {
                    AidaValue::Number(value) => json!(value),
                    AidaValue::Text(value) => json!(value),
                },
                "unit": sensor.unit,
                "color": sensor.color,
            }),
        );
    }
    let body = Value::Object(map);
    if let Some(callback) = query.callback {
        if !valid_callback(&callback) {
            return (axum::http::StatusCode::BAD_REQUEST, "invalid callback").into_response();
        }
        let payload = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
        (
            [(header::CONTENT_TYPE, "application/javascript")],
            format!("{callback}({payload});"),
        )
            .into_response()
    } else {
        Json(body).into_response()
    }
}

async fn sse_handler(
    State(state): State<AidaState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = stream::unfold(0u64, move |counter| {
        let state = state.clone();
        async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let snapshot = snapshot_or_empty(&state).await;
            let data = if snapshot.sensors.is_empty() {
                "Page0|{|}SERROR|sensor data unavailable{|}".to_string()
            } else {
                encode_sse(&snapshot)
            };
            Some((Ok(Event::default().data(data)), counter + 1))
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

async fn panel_html(State(state): State<AidaState>) -> Html<String> {
    let snapshot = snapshot_or_empty(&state).await;
    Html(render_panel_html(&snapshot))
}

/// Render an AIDA64 RemoteSensor-compatible HTML panel.
///
/// The `SimpleN` ids match the SSE payload order so AIDA viewers that discover
/// ids from the HTML page update correctly.
pub fn render_panel_html(snapshot: &AidaSnapshot) -> String {
    let mut items = String::new();
    for (index, sensor) in snapshot.sensors.iter().enumerate() {
        let _ = write!(
            items,
            r#"<div class="item"><span class="label">{label}</span><span class="value" id="Simple{id}">{value}</span><span class="unit">{unit}</span></div>"#,
            label = html_escape(&sensor.label),
            id = index + 1,
            value = html_escape(&sensor.value.display()),
            unit = html_escape(&sensor.unit),
        );
    }
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Ozma AIDA64 Sensors</title>
<style>
body{{background:#0a0a0f;color:#e5e7eb;font-family:system-ui,sans-serif;margin:0;padding:16px}}
.panel{{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:12px}}
.item{{background:#151823;border:1px solid #232838;border-radius:10px;padding:12px;display:flex;align-items:baseline;gap:8px}}
.label{{color:#9aa4b2;font-size:13px;flex:1}}.value{{font-size:24px;font-weight:600}}.unit{{color:#6b7280;font-size:12px}}
</style></head><body><div class="panel">{items}</div>
<script>
const source=new EventSource('/sse');
source.onmessage=(event)=>{{
  if(!event.data.startsWith('Page0|{{|}}'))return;
  const parts=event.data.slice(9).split('{{|}}');
  for(const part of parts){{
    const [id,value]=part.split('|'); if(!id||value===undefined)continue;
    const node=document.getElementById(id); if(node)node.textContent=value;
  }}
}};
</script></body></html>"#
    )
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Generate an Ozma `ScreenLayout` JSON document using the existing widget
/// schema. Metric keys use `@active.<AIDA id>` so the controller's screen
/// manager can resolve them from the active node's metric source.
/// Build the semantic `WidgetIR` projection for the AIDA64 panel.
///
/// The IR retains labels, units, ranges, thresholds, layout, and styling, then
/// [`panel_layout`] maps it to the existing `ScreenLayout` schema. HTML and SSE
/// output remain independent of this projection.
pub fn panel_ir_widgets(snapshot: &AidaSnapshot) -> Vec<WidgetIR> {
    let parsed = serde_json::to_value(snapshot).ok();
    let mut hasher = Sha256::new();
    hasher.update(canonical_json(&parsed.clone().unwrap_or(Value::Null)).as_bytes());
    let source = SourceDocument {
        format: "aida64.remote_sensor".to_owned(),
        version: Some("1".to_owned()),
        source_hash: format!("{:x}", hasher.finalize()),
        original: None,
        parsed,
        unknown: None,
    };

    let mut widgets = Vec::new();
    for (id, label, unit, min, max, x, y, w, h, kind) in [
        ("TCPU", "CPU", "°C", 0.0, 100.0, 20, 20, 140, 140, "gauge"),
        (
            "SCPUUTI", "CPU LOAD", "%", 0.0, 100.0, 180, 20, 140, 140, "gauge",
        ),
        ("TGPU1", "GPU", "°C", 0.0, 100.0, 340, 20, 140, 140, "gauge"),
        (
            "SGPU1UTI", "GPU LOAD", "%", 0.0, 100.0, 500, 20, 140, 140, "gauge",
        ),
    ] {
        if snapshot.by_id(id).is_none() {
            continue;
        }
        let thresholds = if unit == "°C" {
            vec![
                Threshold {
                    at: 80.0,
                    severity: "warning".to_owned(),
                    label: None,
                    color: Some("#f0b94a".to_owned()),
                },
                Threshold {
                    at: 90.0,
                    severity: "critical".to_owned(),
                    label: None,
                    color: Some("#f26464".to_owned()),
                },
            ]
        } else {
            Vec::new()
        };
        widgets.push(panel_widget(
            &source,
            id,
            kind,
            label,
            unit,
            Some(min),
            Some(max),
            (f64::from(x), f64::from(y), f64::from(w), f64::from(h)),
            &color_for("temperature"),
            None,
            thresholds,
        ));
    }

    if snapshot.by_id("SMEMUTI").is_some() {
        widgets.push(panel_widget(
            &source,
            "SMEMUTI",
            "bar",
            "RAM",
            "%",
            Some(0.0),
            Some(100.0),
            (20.0, 190.0, 620.0, 24.0),
            "#a78bfa",
            None,
            Vec::new(),
        ));
    }

    let mut y = 240.0;
    for (id, label, unit, color) in [
        ("SUSEDMEM", "Used Memory", "MB", "#38bdf8"),
        ("SFREEMEM", "Free Memory", "MB", "#3ecf8e"),
        ("PCPUPKG", "CPU Power", "W", "#f26464"),
        ("PGPU1", "GPU Power", "W", "#f0b94a"),
        ("FCPU", "CPU Fan", "RPM", "#38bdf8"),
        ("FGPU1", "GPU Fan", "RPM", "#3ecf8e"),
    ] {
        if snapshot.by_id(id).is_none() {
            continue;
        }
        widgets.push(panel_widget(
            &source,
            id,
            "number",
            label,
            unit,
            None,
            None,
            (20.0, y, 290.0, 56.0),
            color,
            Some(24.0),
            Vec::new(),
        ));
        y += 64.0;
        if y > 440.0 {
            break;
        }
    }

    if let Some(first) = widgets.first_mut() {
        let extension = first
            .extensions
            .entry("screen_layout")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(object) = extension.as_object_mut() {
            object.insert(
                "layout".to_owned(),
                json!({
                    "background": "#0a0a0f",
                    "refresh_hz": 2,
                }),
            );
        }
    }
    widgets
}

#[allow(clippy::too_many_arguments)]
fn panel_widget(
    source: &SourceDocument,
    id: &str,
    kind: &str,
    label: &str,
    unit: &str,
    min: Option<f64>,
    max: Option<f64>,
    (x, y, width, height): (f64, f64, f64, f64),
    color: &str,
    font_size: Option<f64>,
    thresholds: Vec<Threshold>,
) -> WidgetIR {
    let mut tokens = Map::new();
    tokens.insert("color".to_owned(), Value::String(color.to_owned()));
    let mut extensions = Map::new();
    extensions.insert(
        "screen_layout".to_owned(),
        json!({"widget": {"color": color}}),
    );
    WidgetIR {
        id: format!("aida64-{}", id.to_ascii_lowercase()),
        kind: kind.to_owned(),
        role: Some(id.to_ascii_lowercase()),
        binding: DataBinding {
            metric: format!("@active.{id}"),
            source: Some("aida64".to_owned()),
            unit: Some(unit.to_owned()),
            scale: None,
            format: None,
            field_id: None,
            value_type: Some("real".to_owned()),
            property: None,
        },
        domain: Some(ValueDomain {
            min,
            max,
            nominal: None,
            thresholds,
            severity_bands: Vec::new(),
        }),
        history: None,
        label: Some(label.to_owned()),
        icon: None,
        actions: Vec::new(),
        accessibility: None,
        layout: Some(LayoutHint {
            x: Some(x),
            y: Some(y),
            width: Some(width),
            height: Some(height),
            zone: None,
            responsive_priority: None,
            aspect_ratio: None,
        }),
        style: Some(StyleRef {
            theme: None,
            tokens,
            classes: Vec::new(),
        }),
        typography: font_size.map(|size| TypographyRef {
            size_px: Some(size),
            ..TypographyRef::default()
        }),
        fonts: Vec::new(),
        vibe: None,
        shape: None,
        source: source.clone(),
        extensions,
        assets: Vec::new(),
    }
}

/// AIDA64-compatible Ozma panel layout generated from semantic widget IR.
pub fn panel_layout(snapshot: &AidaSnapshot) -> Value {
    let widgets = panel_ir_widgets(snapshot);
    let mut layout = widget_irs_to_screen_layout("aida64-panel", &widgets);
    if let Some(document) = layout.as_object_mut() {
        document.insert(
            "name".to_owned(),
            Value::String("AIDA64 Sensor Panel".to_owned()),
        );
        document.insert("background".to_owned(), Value::String("#0a0a0f".to_owned()));
        document.insert("refresh_hz".to_owned(), Value::from(2));
    }
    layout
}

/// Run the AIDA64 RemoteSensor listener until cancellation.
pub async fn serve(
    bind: &str,
    port: u16,
    hardware: HardwareState,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let app = router(hardware);
    let listener = tokio::net::TcpListener::bind((bind, port)).await?;
    tracing::info!(bind, port, "AIDA64 RemoteSensor listener running");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_hardware::{
        CpuCoreInfo, CpuPackageInfo, FanReading, GpuInfo, HardwareSnapshot, MotherboardInfo,
        NetworkInfo, StorageInfo, VoltageReading,
    };

    fn hardware() -> HardwareSnapshot {
        HardwareSnapshot {
            timestamp: 1_700_000_000.0,
            cpu: CpuPackageInfo {
                brand: "Test CPU".into(),
                package_temp_c: 62.0,
                base_clock_mhz: 3000.0,
                boost_clock_mhz: 4500.0,
                core_voltage_v: 1.25,
                package_power_w: 95.0,
                cores: vec![CpuCoreInfo {
                    index: 0,
                    temperature_c: 60.0,
                    clock_mhz: 4200.0,
                    voltage_v: 1.25,
                    usage_percent: 21.0,
                    power_w: 15.0,
                }],
                ..CpuPackageInfo::default()
            },
            gpus: vec![GpuInfo {
                index: 0,
                model: "Test GPU".into(),
                temperature_c: 55.0,
                hotspot_temp_c: 68.0,
                core_clock_mhz: 1900.0,
                memory_clock_mhz: 1200.0,
                power_w: 180.0,
                fan_rpm: 1400,
                utilization_percent: 44.0,
                ..GpuInfo::default()
            }],
            storage: vec![StorageInfo {
                model: "Test SSD".into(),
                temperature_c: 41.0,
                read_rate_mbs: 12.0,
                write_rate_mbs: 3.0,
                ..StorageInfo::default()
            }],
            motherboard: MotherboardInfo {
                model: "Test Board".into(),
                bios_version: "1.2.3".into(),
                vrm_temp_c: 48.0,
                ..MotherboardInfo::default()
            },
            fans: vec![
                FanReading {
                    name: "CPU Fan".into(),
                    rpm: 1200,
                    ..FanReading::default()
                },
                FanReading {
                    name: "Chassis Fan".into(),
                    rpm: 800,
                    ..FanReading::default()
                },
            ],
            voltages: vec![VoltageReading {
                name: "+12V".into(),
                voltage_v: 12.1,
                ..VoltageReading::default()
            }],
            network: vec![NetworkInfo {
                name: "eth0".into(),
                rx_rate_mbs: 2.0,
                tx_rate_mbs: 1.0,
                rx_bytes: 1024 * 1024,
                tx_bytes: 512 * 1024,
                ..NetworkInfo::default()
            }],
            ..HardwareSnapshot::default()
        }
    }

    fn usage() -> SystemUsage {
        SystemUsage {
            cpu_percent: 21.5,
            per_core_percent: vec![21.5],
            used_mib: 8192.0,
            total_mib: 16384.0,
            uptime_secs: 3723,
            processes: 123,
        }
    }

    fn ids(snapshot: &AidaSnapshot) -> std::collections::BTreeSet<&str> {
        snapshot
            .sensors
            .iter()
            .map(|sensor| sensor.id.as_str())
            .collect()
    }

    #[test]
    fn maps_documented_sensor_ids() {
        let snapshot = build_snapshot_with_usage(&hardware(), &usage());
        let ids = ids(&snapshot);
        for id in [
            "TCPU",
            "TCPUPKG",
            "TCC-1-01",
            "SCPUUTI",
            "SMEMUTI",
            "SUSEDMEM",
            "TGPU1",
            "TGPU1HOT",
            "SGPU1UTI",
            "FCPU",
            "FCHA1",
            "VCPU",
            "VP12V",
            "PCPUPKG",
            "PGPU1",
            "THDD1",
            "SDSK1READSPD",
            "SNIC1DL",
        ] {
            assert!(ids.contains(id), "missing {id}");
        }
        assert!(
            ids.contains("FGPU1"),
            "GPU fan should map to FGPU1 when rpm is exposed"
        );
    }

    #[test]
    fn numeric_metrics_and_panel_values_are_available() {
        let snapshot = build_snapshot_with_usage(&hardware(), &usage());
        let metrics = snapshot.numeric_metrics();
        assert!(metrics["TCPU"] > 60.0);
        assert!(metrics["SMEMUTI"] > 49.0 && metrics["SMEMUTI"] < 51.0);
        let values = snapshot.panel_values();
        assert!(values["@active.TCPU"].is_number());
        assert!(values["@active.SDATE"].is_string());
    }

    #[test]
    fn sse_has_page_and_simple_ids() {
        let snapshot = build_snapshot_with_usage(&hardware(), &usage());
        let line = encode_sse(&snapshot);
        assert!(line.starts_with("Page0|{|}"));
        assert!(line.contains("Simple1|"));
        assert!(line.ends_with("{|}"));
        assert!(!line.contains('|',) || line.matches("{|}").count() > 2);
    }

    #[test]
    fn panel_layout_uses_existing_widget_schema() {
        let snapshot = build_snapshot_with_usage(&hardware(), &usage());
        let layout = panel_layout(&snapshot);
        assert_eq!(layout["id"], "aida64-panel");
        assert_eq!(layout["name"], "AIDA64 Sensor Panel");
        assert!(layout["widgets"].as_array().unwrap().len() >= 4);
        let fields = layout["fields"].as_array().unwrap();
        assert!(fields
            .iter()
            .any(|field| { field["key"] == "@active.TCPU" && field["unit"] == "°C" }));
        assert!(fields.iter().any(|field| field["key"] == "@active.SMEMUTI"));
        let types: std::collections::BTreeSet<_> = layout["widgets"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|widget| widget["type"].as_str())
            .collect();
        assert!(types.contains("gauge"));
        assert!(types.contains("bar"));
        assert_eq!(layout["widgets"][0]["ir_kind"], "gauge");
        assert_eq!(layout["background"], "#0a0a0f");
        assert_eq!(layout["refresh_hz"], 2);
    }

    #[test]
    fn panel_widget_ir_is_valid_and_preserves_semantics() {
        let snapshot = build_snapshot_with_usage(&hardware(), &usage());
        let widgets = panel_ir_widgets(&snapshot);
        assert!(!widgets.is_empty());
        for widget in &widgets {
            widget.validate().unwrap();
            assert_eq!(widget.source.format, "aida64.remote_sensor");
            assert_eq!(widget.source.source_hash.len(), 64);
        }
        let cpu = widgets
            .iter()
            .find(|widget| widget.binding.metric == "@active.TCPU")
            .unwrap();
        assert_eq!(cpu.kind, "gauge");
        assert_eq!(cpu.domain.as_ref().unwrap().max, Some(100.0));
        let thresholds = &cpu.domain.as_ref().unwrap().thresholds;
        assert_eq!(thresholds.len(), 2);
        assert_eq!(thresholds[1].severity, "critical");
        assert_eq!(cpu.layout.as_ref().unwrap().width, Some(140.0));

        let layout = panel_layout(&snapshot);
        let widget = &layout["widgets"][0];
        assert_eq!(widget["metric"], "@active.TCPU");
        assert_eq!(widget["warn_color"], "#f0b94a");
        assert_eq!(widget["crit_color"], "#f26464");
        assert_eq!(widget["color"], color_for("temperature"));
    }

    #[test]
    fn html_panel_has_simple_spans_and_escapes_labels() {
        let mut snapshot = build_snapshot_with_usage(&hardware(), &usage());
        snapshot.sensors.push(AidaSensor {
            id: "XSS".into(),
            label: "<script>".into(),
            value: AidaValue::Text("bad".into()),
            unit: String::new(),
            kind: "system".into(),
            color: "#fff".into(),
        });
        let html = render_panel_html(&snapshot);
        assert!(html.contains("id=\"Simple1\""));
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>bad</script>"));
    }

    #[test]
    fn api_selection_helpers_are_stable() {
        let snapshot = build_snapshot_with_usage(&hardware(), &usage());
        assert!(snapshot.by_id("TCPU").is_some());
        assert!(snapshot.by_id("does-not-exist").is_none());
        assert!(valid_callback("myCallback"));
        assert!(!valid_callback("bad;alert(1)"));
    }
}
