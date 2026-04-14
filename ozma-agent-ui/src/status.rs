// SPDX-License-Identifier: AGPL-3.0-only
//! Status window — shown when user clicks "Show Status" from the tray.
//!
//! Sections:
//!  1. Connection banner  (top bar)
//!  2. Agent info row
//!  3. Recent actions table (scrollable, last 20)
//!  4. Mesh peers (collapsible)
//!  5. Quick action footer
//!
//! Data refresh: every 5s via GET /api/v1/status
//! Live events: WS channel updates action log immediately.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Color32, RichText, ScrollArea, TopBottomPanel, Ui, Vec2, Widget,
};
use serde::Deserialize;
use tokio::sync::RwLock;

// ─────────────────────────────────────────────────────────────────────────────
// Types
// ─────────────────────────────────────────────────────────────────────────────

/// Coloured dot matches this state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    Connected,
    Connecting,
    Paused,
    Error,
}

impl TrayState {
    fn color(&self) -> Color32 {
        match self {
            Self::Connected => Color32::from_rgb(76, 175, 80),
            Self::Connecting => Color32::from_rgb(255, 193, 7),
            Self::Paused => Color32::from_rgb(158, 158, 158),
            Self::Error => Color32::from_rgb(244, 67, 54),
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Connected => "Connected",
            Self::Connecting => "Connecting…",
            Self::Paused => "Paused",
            Self::Error => "Error",
        }
    }
}

/// Result of an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionResult {
    Approved,
    Denied,
    Auto,
}

impl ActionResult {
    fn icon(&self) -> &'static str {
        match self {
            Self::Approved => "✓",
            Self::Denied => "✗",
            Self::Auto => "⚡",
        }
    }

    fn color(&self) -> Color32 {
        match self {
            Self::Approved => Color32::from_rgb(76, 175, 80),
            Self::Denied => Color32::from_rgb(244, 67, 54),
            Self::Auto => Color32::from_rgb(255, 193, 7),
        }
    }
}

/// A single row in the recent actions log.
#[derive(Debug, Clone)]
pub struct ActionLogEntry {
    pub action_type: String,
    pub description: String,
    pub result: ActionResult,
    pub timestamp: Instant,
}

impl ActionLogEntry {
    fn time_ago(&self) -> String {
        let secs = self.timestamp.elapsed().as_secs();
        if secs < 60 {
            format!("{}s ago", secs)
        } else {
            let mins = secs / 60;
            if mins < 60 {
                format!("{}m ago", mins)
            } else {
                let hours = mins / 60;
                format!("{}h ago", hours)
            }
        }
    }

    fn icon(&self) -> &'static str {
        match self.action_type.to_lowercase().as_str() {
            s if s.contains("screen") || s.contains("capture") => "🖥",
            s if s.contains("keyboard") || s.contains("key") => "⌨",
            s if s.contains("mouse") || s.contains("click") => "🖱",
            s if s.contains("network") || s.contains("http") => "🌐",
            s if s.contains("clipboard") => "📋",
            s if s.contains("audio") => "🔊",
            s if s.contains("file") => "📁",
            _ => "⚙",
        }
    }
}

/// Peer info from /api/v1/status → mesh_peers.
#[derive(Debug, Clone, Deserialize)]
pub struct MeshPeer {
    pub name: String,
    pub host: String,
    pub latency_ms: Option<u32>,
}

/// Controller status response.
#[derive(Debug, Clone, Deserialize)]
pub struct StatusResponse {
    #[serde(rename = "active_node_id")]
    pub active_node_id: Option<String>,
    pub nodes: Option<std::collections::HashMap<String, NodeInfo>>,
    #[serde(rename = "active_scenario_id")]
    pub active_scenario_id: Option<String>,
    #[serde(rename = "mesh_peers")]
    pub mesh_peers: Option<Vec<MeshPeer>>,
}

/// Minimal node info from status snapshot.
#[derive(Debug, Clone, Deserialize)]
pub struct NodeInfo {
    pub id: String,
    pub host: String,
    pub port: Option<u16>,
    pub role: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared State (for integration with main app)
// ─────────────────────────────────────────────────────────────────────────────

/// Stored by the app; shared between tray, status window, etc.
#[derive(Debug, Clone)]
pub struct SharedState {
    pub tray_state: TrayState,
    pub controller_url: String,
    pub latency_ms: Option<u32>,
    pub agent_version: String,
    pub node_name: String,
    pub wg_port: u16,
    pub uptime: Duration,
    pub action_log: VecDeque<ActionLogEntry>,
    pub mesh_peers: Vec<MeshPeer>,
    pub paused: bool,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            tray_state: TrayState::Connecting,
            controller_url: "http://localhost:7380".into(),
            latency_ms: None,
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            node_name: String::new(),
            wg_port: 51820,
            uptime: Duration::ZERO,
            action_log: VecDeque::with_capacity(20),
            mesh_peers: Vec::new(),
            paused: false,
        }
    }
}

/// Thread-safe shared state wrapper for async access.
pub type SharedStateHandle = Arc<RwLock<SharedState>>;

/// Create a new shared state handle.
pub fn create_shared_state() -> SharedStateHandle {
    Arc::new(RwLock::new(SharedState::default()))
}

/// Add an action log entry to the shared state.
pub async fn push_action_log(
    shared: &SharedStateHandle,
    action_type: String,
    description: String,
    result: ActionResult,
) {
    let entry = ActionLogEntry {
        action_type,
        description,
        result,
        timestamp: Instant::now(),
    };

    let mut state = shared.write().await;
    if state.action_log.len() >= 20 {
        state.action_log.pop_front();
    }
    state.action_log.push_back(entry);
}

/// Create a synthetic test approval entry.
pub async fn inject_test_approval(shared: &SharedStateHandle) {
    push_action_log(
        shared,
        "Screen Capture".to_string(),
        "Machine A full desktop".to_string(),
        ActionResult::Approved,
    )
    .await;
}

// ─────────────────────────────────────────────────────────────────────────────
// StatusWindow
// ─────────────────────────────────────────────────────────────────────────────

pub struct StatusWindow {
    shared: SharedStateHandle,
    last_poll: Instant,
    last_latency_check: Instant,
    mesh_peers_open: bool,
    start_time: Instant,
    latest_status: Arc<RwLock<Option<StatusResponse>>>,
    should_close: Arc<AtomicBool>,
}

impl StatusWindow {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        shared: SharedStateHandle,
    ) -> Self {
        load_custom_fonts(cc);

        Self {
            shared,
            last_poll: Instant::now() - Duration::from_secs(5),
            last_latency_check: Instant::now() - Duration::from_secs(6),
            mesh_peers_open: false,
            start_time: Instant::now(),
            latest_status: Arc::new(RwLock::new(None)),
            should_close: Arc::new(AtomicBool::new(false)),
        }
    }

    fn poll_status(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_poll) < Duration::from_secs(5) {
            return;
        }
        self.last_poll = now;

        let url = {
            let s = self.shared.blocking_read();
            s.controller_url.clone()
        };

        let status_store = Arc::clone(&self.latest_status);
        let shared_clone = Arc::clone(&self.shared);

        std::thread::spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .ok();

            if let Some(client) = client {
                if let Ok(resp) = client.get(format!("{}/api/v1/status", url)).send() {
                    if let Ok(status) = resp.json::<StatusResponse>() {
                        let status_store2 = status_store.clone();
                        std::thread::spawn(move || {
                            let rt = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build();
                            if let Ok(rt) = rt {
                                rt.block_on(async {
                                    let mut guard = status_store2.write().await;
                                    *guard = Some(status);
                                });
                            }
                        });

                        let peers = status.mesh_peers.unwrap_or_default();
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build();
                        if let Ok(rt) = rt {
                            rt.block_on(async {
                                let mut shared = shared_clone.write().await;
                                shared.mesh_peers = peers;
                            });
                        }
                    }
                }
            }
        });
    }

    fn measure_latency(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_latency_check) < Duration::from_secs(5) {
            return;
        }
        self.last_latency_check = now;

        let url = {
            let s = self.shared.blocking_read();
            s.controller_url.clone()
        };

        let shared_clone = Arc::clone(&self.shared);

        std::thread::spawn(move || {
            let start = Instant::now();
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .ok();

            if let Some(client) = client {
                if client.get(format!("{}/healthz", url)).send().is_ok() {
                    let latency = start.elapsed().as_millis() as u32;
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    if let Ok(rt) = rt {
                        rt.block_on(async {
                            let mut shared = shared_clone.write().await;
                            shared.latency_ms = Some(latency);
                            if shared.tray_state == TrayState::Connecting {
                                shared.tray_state = TrayState::Connected;
                            }
                        });
                    }
                } else {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    if let Ok(rt) = rt {
                        rt.block_on(async {
                            let mut shared = shared_clone.write().await;
                            if !shared.paused {
                                shared.tray_state = TrayState::Error;
                            }
                        });
                    }
                }
            }
        });
    }

    fn draw_connection_banner(&self, ui: &mut Ui) {
        let state = self.shared.blocking_read();
        let tray_color = state.tray_state.color();
        let tray_label = state.tray_state.label();
        let latency = state.latency_ms.map(|ms| format!("{}ms", ms)).unwrap_or_else(|| "—".to_string());
        let uptime_str = format_uptime(state.uptime);

        ui.horizontal(|ui| {
            ui.colored_label(tray_color, "●");
            ui.label(RichText::new(format!("Connected to {}", state.controller_url)).small());
            ui.label("|");
            ui.label(RichText::new(format!("Latency: {}", latency)).small());
            ui.label("|");
            ui.label(RichText::new(format!("Uptime: {}", uptime_str)).small());
        });
    }

    fn draw_agent_info(&self, ui: &mut Ui) {
        let state = self.shared.blocking_read();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!(
                "{} v{}  |  Node: {}  |  WG port: {}",
                "ozma-agent",
                state.agent_version,
                state.node_name.trim_end_matches('.'),
                state.wg_port,
            ))
            .small());
        });
    }

    fn draw_action_log(&self, ui: &mut Ui) {
        let state = self.shared.blocking_read();
        let entries: Vec<_> = state.action_log.iter().rev().collect();

        ui.label(RichText::new("Recent Actions").strong());
        ScrollArea::vertical()
            .max_height(150.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                // Header
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Type").small().weak());
                    ui.label(RichText::new("Description").small().weak());
                    ui.label(RichText::new("Result").small().weak());
                    ui.label(RichText::new("Time").small().weak());
                });
                ui.separator();
                for entry in entries {
                    ui.horizontal(|ui| {
                        ui.label(format!("{} {}", entry.icon(), entry.action_type));
                        ui.label(entry.description.trim_end_matches(".local").to_string());
                        ui.colored_label(entry.result.color(), entry.result.icon());
                        ui.label(entry.time_ago());
                    });
                }
                if entries.is_empty() {
                    ui.label(RichText::new("(no recent actions)").weak().small());
                }
            });
    }

    fn draw_mesh_peers(&mut self, ui: &mut Ui) {
        let state = self.shared.blocking_read();
        let peer_count = state.mesh_peers.len();

        let header_text = if peer_count == 0 {
            "Mesh Peers (0)".to_string()
        } else {
            format!("Mesh Peers ({})", peer_count)
        };

        ui.horizontal(|ui| {
            if ui.button(if self.mesh_peers_open { "▼" } else { "▶" }).clicked() {
                self.mesh_peers_open = !self.mesh_peers_open;
            }
            ui.label(RichText::new(header_text).strong());
        });

        if self.mesh_peers_open {
            for peer in &state.mesh_peers {
                ui.horizontal(|ui| {
                    let name = peer.name.trim_end_matches(".local").trim_end_matches("._ozma._udp.local.");
                    ui.label(name);
                    ui.label(peer.host.clone());
                    let latency_str = peer
                        .latency_ms
                        .map(|ms| format!("↕ {}ms", ms))
                        .unwrap_or_else(|| "—".to_string());
                    ui.label(latency_str);
                });
            }
            if state.mesh_peers.is_empty() {
                ui.label(RichText::new("(no peers connected)").weak().small());
            }
        }
    }

    fn draw_footer(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if ui.button("⏸ Pause Agent").clicked() {
                let shared_clone = Arc::clone(&self.shared);
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    if let Ok(rt) = rt {
                        rt.block_on(async {
                            let mut s = shared_clone.write().await;
                            s.paused = true;
                            s.tray_state = TrayState::Paused;
                        });
                    }
                });
            }
            if ui.button("⟳ Reconnect").clicked() {
                let shared_clone = Arc::clone(&self.shared);
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    if let Ok(rt) = rt {
                        rt.block_on(async {
                            let mut s = shared_clone.write().await;
                            s.tray_state = TrayState::Connecting;
                            s.paused = false;
                        });
                    }
                });
            }
            if ui.button("Settings").clicked() {
                // Settings action would be handled by the app
            }
        });
    }

    #[cfg(debug_assertions)]
    fn draw_test_button(&mut self, ui: &mut Ui) {
        if ui.button("🧪 Inject Test Approval").clicked() {
            let shared_clone = Arc::clone(&self.shared);
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                if let Ok(rt) = rt {
                    rt.block_on(async {
                        inject_test_approval(&shared_clone).await;
                    });
                }
            });
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn format_uptime(duration: Duration) -> String {
    let secs = duration.as_secs();
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    format!("{}h {}m", hours, mins)
}

fn load_custom_fonts(_cc: &eframe::CreationContext<'_>) {
    #[cfg(feature = "embed-fonts")]
    {
        let mut fonts = _cc.egui_ctx.memory().fonts_mut();
        // Load Inter from embedded bytes if available.
        // When assets are bundled, include_bytes! resolves at compile time.
        // Falls back gracefully when not bundled.
    }
    #[cfg(not(feature = "embed-fonts"))]
    {
        // No-op when fonts aren't bundled.
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// eframe integration
// ─────────────────────────────────────────────────────────────────────────────

impl eframe::App for StatusWindow {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_status();
        self.measure_latency();

        // Update uptime from start_time
        let uptime = self.start_time.elapsed();
        {
            let mut state = self.shared.blocking_write();
            state.uptime = uptime;
        }

        // ── Top panel: connection banner ──────────────────────────────────────
        TopBottomPanel::top("connection_banner")
            .resizable(false)
            .show(ctx, |ui| {
                ui.set_height(30.0);
                self.draw_connection_banner(ui);
            });

        // ── Central panel: agent info, action log, mesh peers ─────────────────
        egui::CentralPanel::default()
            .frame(egui::Frame::default().inner_margin(8.0))
            .show(ctx, |ui| {
                ui.set_width(480.0);
                ui.add_space(8.0);
                self.draw_agent_info(ui);
                ui.separator();
                self.draw_action_log(ui);
                ui.separator();
                self.draw_mesh_peers(ui);
                ui.add_space(8.0);

                #[cfg(debug_assertions)]
                {
                    self.draw_test_button(ui);
                }

                self.draw_footer(ui);
            });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Window builder helper
// ─────────────────────────────────────────────────────────────────────────────

use eframe::egui::ViewportBuilder;

/// Build the viewport for the status window.
pub fn make_viewport() -> ViewportBuilder {
    ViewportBuilder::default()
        .with_title("Ozma Agent — Status")
        .with_inner_size([520.0, 400.0])
        .with_min_inner_size([400.0, 300.0])
}
