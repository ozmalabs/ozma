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
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};
use eframe::egui::{
    self, Color32, FontId, Grid, Layout, RichText, ScrollArea, TextStyle,
    TopBottomPanel, Ui, Vec2, Widget,
};
use serde::Deserialize;

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
            Self::Connected => Color32::from_rgb(76, 175, 80),   // green
            Self::Connecting => Color32::from_rgb(255, 193, 7),  // amber
            Self::Paused => Color32::from_rgb(158, 158, 158),     // grey
            Self::Error => Color32::from_rgb(244, 67, 54),        // red
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
    #[serde(rename = "role")]
    pub role: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// State
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

// ─────────────────────────────────────────────────────────────────────────────
// StatusWindow
// ─────────────────────────────────────────────────────────────────────────────

pub struct StatusWindow {
    /// Shared mutable state (set by app, read by UI).
    shared: std::sync::Arc<tokio::sync::RwLock<SharedState>>,
    /// Last time we polled /api/v1/status.
    last_poll: Instant,
    /// Last time we measured latency (ping).
    last_latency_check: Instant,
    /// Collapsed state for mesh peers section.
    mesh_peers_open: bool,
    /// Uptime start time.
    start_time: Instant,
    /// Latest status response for extracting mesh peers.
    latest_status: std::sync::Arc<tokio::sync::RwLock<Option<StatusResponse>>>,
}

impl StatusWindow {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        shared: std::sync::Arc<tokio::sync::RwLock<SharedState>>,
    ) -> Self {
        let fonts = &mut cc.egui_ctx.memory().fonts;
        fonts
            .definitely_add_font(&include_bytes!("../assets/Inter-SemiBold.ttf")[..]);
        fonts
            .definitely_add_font(&include_bytes!("../assets/JetBrainsMono-Regular.ttf")[..]);

        Self {
            shared,
            last_poll: Instant::now() - Duration::from_secs(5), // poll immediately on first frame
            last_latency_check: Instant::now() - Duration::from_secs(6), // ping immediately
            mesh_peers_open: false,
            start_time: Instant::now(),
            latest_status: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    fn poll_status(&mut self, ui: &mut Ui) {
        let now = Instant::now();
        if now.duration_since(self.last_poll) < Duration::from_secs(5) {
            return;
        }
        self.last_poll = now;

        let url = {
            let s = self.shared.blocking_read();
            s.controller_url.clone()
        };

        let status_store = self.latest_status.clone();
        let shared_clone = std::sync::Arc::clone(&self.shared);

        // Spawn HTTP GET for status — use tokio runtime from the app
        std::thread::spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .ok();
            let client = match client {
                Some(c) => c,
                None => return,
            };

            // Fetch status
            if let Ok(resp) = client.get(format!("{}/api/v1/status", url)).send() {
                if let Ok(status) = resp.json::<StatusResponse>() {
                    // Store the status for mesh peers
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

                    // Update mesh peers in shared state
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

        let shared_clone = std::sync::Arc::clone(&self.shared);

        std::thread::spawn(move || {
            let start = Instant::now();
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .ok();
            let client = match client else { return };

            if client.get(format!("{}/health", url)).send().is_ok() {
                let latency = start.elapsed().as_millis() as u32;
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                if let Ok(rt) = rt {
                    rt.block_on(async {
                        let mut shared = shared_clone.write().await;
                        shared.latency_ms = Some(latency);
                    });
                }
            }
        });
    }
}

impl eframe::App for StatusWindow {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Measure latency every 5s
        self.measure_latency();

        // Refresh every 5s even if no user interaction.
        self.poll_status(ctx);

        // Update uptime in shared state
        let uptime = self.start_time.elapsed();

        // ── Top bar ──────────────────────────────────────────────────────────────
        TopBottomPanel::top("connection_banner")
            .frame(egui::Frame::dark_panel().fill(Color32::from_rgba_unmultiplied(18, 18, 28, 255)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    // Status dot
                    let (dot_color, label, latency_str, uptime_str) = {
                        let s = self.shared.blocking_read();
                        (
                            s.tray_state.color(),
                            s.tray_state.label(),
                            s.latency_ms.map(|ms| format!("{ms}ms")).unwrap_or_else(|| "—".into()),
                            format_uptime(uptime),
                        )
                    };

                    // Use a filled circle as the status indicator
                    let dot = egui::paint::CircleShape::filled(6.0, dot_color);
                    ui.add(egui::widgets::Image::new(
                        egui::include_image!("../assets/status_dot.png"),
                        egui::Vec2::splat(12.0),
                    ));

                    let controller_url = {
                        let s = self.shared.blocking_read();
                        s.controller_url.clone()
                    };

                    let text = format!(
                        "🟢 {} to {}  |  Latency: {}  |  Uptime: {}",
                        label,
                        controller_url,
                        latency_str,
                        uptime_str,
                    );
                    ui.label(RichText::new(text).size(14.0).color(Color32::WHITE));
                });
            });

        // ── Main panel ──────────────────────────────────────────────────────────
        egui::CentralPanel::default()
            .frame(egui::Frame::dark_panel().fill(Color32::from_rgba_unmultiplied(12, 12, 18, 255)))
            .show(ctx, |ui| {
                ui.set_width(ui.available_width());
                self.draw_agent_info(ui);
                ui.add_space(8.0);
                self.draw_action_log(ui);
                ui.add_space(8.0);
                self.draw_mesh_peers(ui);
                ui.add_space(8.0);
                self.draw_footer(ui, ctx);
            });

        // ── Debug: test injection ───────────────────────────────────────────────
        #[cfg(debug_assertions)]
        {
            egui::Area::new("debug_test_btn")
                .anchor(egui::Align::BOTTOM_RIGHT, [-10.0, -10.0])
                .show(ctx, |ui| {
                    if ui
                        .button("🧪 Inject Test Approval")
                        .on_hover_text("Click to push a synthetic ScreenCapture approval request")
                        .clicked()
                    {
                        let mut entry = ActionLogEntry {
                            action_type: "Screen Capture".into(),
                            description: "Machine A full desktop".into(),
                            result: ActionResult::Approved,
                            timestamp: Instant::now(),
                        };
                        let mut s = self.shared.blocking_write();
                        if s.action_log.len() >= 20 {
                            s.action_log.pop_front();
                        }
                        s.action_log.push_back(entry);
                    }
                });
        }

        // Re-schedule in 5s.
        let ctx2 = ctx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(5));
            ctx2.request_repaint();
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Sections
// ─────────────────────────────────────────────────────────────────────────────

impl StatusWindow {
    fn draw_agent_info(&self, ui: &mut Ui) {
        let s = self.shared.blocking_read();
        ui.horizontal(|ui| {
            let version_text = format!("ozma-agent v{}", s.agent_version);
            let node_text = format!("Node: {}", s.node_name.if_empty(|| "—"));
            let wg_text = format!("WG port: {}", s.wg_port);

            ui.label(
                RichText::new(format!("{}  |  {}  |  {}", version_text, node_text, wg_text))
                    .size(13.0)
                    .color(Color32::from_gray(180)),
            );
        });
    }

    fn draw_action_log(&self, ui: &mut Ui) {
        ui.label(
            RichText::new("Recent Actions")
                .size(14.0)
                .color(Color32::WHITE)
                .strong(),
        );
        ui.add_space(4.0);

        let log = {
            let s = self.shared.blocking_read();
            s.action_log.iter().rev().take(20).cloned().collect::<Vec<_>>()
        };

        if log.is_empty() {
            ui.label(
                RichText::new("No recent actions.")
                    .size(12.0)
                    .color(Color32::from_gray(120)),
            );
            return;
        }

        // Table header
        ui.horizontal(|ui| {
            ui.set_width(80.0);
            ui.label(bold("Type"));
            ui.add_space(8.0);
            ui.set_width(200.0);
            ui.label(bold("Description"));
            ui.add_space(8.0);
            ui.set_width(80.0);
            ui.label(bold("Result"));
            ui.add_space(8.0);
            ui.label(bold("Time"));
        });

        ui.separator();

        ScrollArea::vertical()
            .max_height(160.0)
            .show(ui, |ui| {
                Grid::new("action_log_grid").show(ui, |ui| {
                    for entry in &log {
                        // Icon + type
                        ui.horizontal(|ui| {
                            ui.label(entry.icon());
                            ui.label(format!(" {}", entry.action_type));
                        });
                        ui.next_column();

                        // Description (truncated)
                        let desc = if entry.description.len() > 35 {
                            format!("{}…", &entry.description[..32])
                        } else {
                            entry.description.clone()
                        };
                        ui.label(RichText::new(desc).size(12.0).color(Color32::from_gray(200)));
                        ui.next_column();

                        // Result with colour
                        let result_text = format!("{} {}", entry.result.icon(), format!("{:?}", entry.result));
                        ui.label(
                            RichText::new(result_text)
                                .size(12.0)
                                .color(entry.result.color()),
                        );
                        ui.next_column();

                        // Time ago
                        ui.label(
                            RichText::new(entry.time_ago())
                                .size(11.0)
                                .color(Color32::from_gray(140)),
                        );
                        ui.next_column();
                    }
                });
            });
    }

    fn draw_mesh_peers(&self, ui: &mut Ui) {
        let peers = {
            let s = self.shared.blocking_read();
            s.mesh_peers.clone()
        };
        let count = peers.len();

        // Header with collapsible toggle
        ui.horizontal(|ui| {
            let label = format!("Mesh Peers ({})", count);
            if ui
                .selectable_label(self.mesh_peers_open, label)
                .on_hover_text("Click to expand/collapse mesh peer list")
                .clicked()
            {
                self.mesh_peers_open = !self.mesh_peers_open;
            }

            // Triangle indicator
            let arrow = if self.mesh_peers_open { "▼" } else { "▶" };
            ui.label(RichText::new(arrow).size(12.0).color(Color32::from_gray(150)));
        });

        if !self.mesh_peers_open {
            return;
        }

        ui.add_space(4.0);

        if peers.is_empty() {
            ui.label(
                RichText::new("No mesh peers connected.")
                    .size(12.0)
                    .color(Color32::from_gray(120)),
            );
            return;
        }

        ScrollArea::vertical()
            .max_height(100.0)
            .show(ui, |ui| {
                for peer in &peers {
                    ui.horizontal(|ui| {
                        // Name
                        ui.label(
                            RichText::new(&peer.name)
                                .size(12.0)
                                .color(Color32::from_rgb(140, 180, 255)),
                        );
                        ui.add_space(12.0);

                        // Host
                        ui.label(
                            RichText::new(&peer.host)
                                .size(11.0)
                                .color(Color32::from_gray(160)),
                        );
                        ui.add_space(12.0);

                        // Latency
                        if let Some(ms) = peer.latency_ms {
                            ui.label(
                                RichText::new(format!("↕ {}ms", ms))
                                    .size(11.0)
                                    .color(Color32::from_rgb(100, 220, 100)),
                            );
                        } else {
                            ui.label(
                                RichText::new("↕ —")
                                    .size(11.0)
                                    .color(Color32::from_gray(100)),
                            );
                        }
                    });
                }
            });
    }

    fn draw_footer(&self, ui: &mut Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            let btn = |label: &str, hover: &str| -> egui::Response {
                ui.add(
                    egui::Button::new(label)
                        .fill(Color32::from_rgba_unmultiplied(30, 30, 45, 255))
                        .stroke(egui::Stroke::new(1.0, Color32::from_gray(80)))
                        .rounding(6.0)
                        .min_size(Vec2::new(110.0, 30.0)),
                )
                .on_hover_text(hover)
            };

            // Pause / Resume
            let (pause_label, pause_hover) = {
                let s = self.shared.blocking_read();
                if s.paused {
                    ("▶ Resume Agent", "Resume the agent (re-enables polling and actions)")
                } else {
                    ("⏸ Pause Agent", "Pause the agent (stops polling and action execution)")
                }
            };
            if btn(pause_label, pause_hover).clicked() {
                let mut s = self.shared.blocking_write();
                s.paused = !s.paused;
                s.tray_state = if s.paused { TrayState::Paused } else { TrayState::Connected };
                // Request repaint to update UI immediately
                ctx.request_repaint();
            }

            ui.add_space(8.0);

            // Reconnect
            if btn("⟳ Reconnect", "Force reconnect to the controller").clicked() {
                // Reset state to connecting
                let mut s = self.shared.blocking_write();
                s.tray_state = TrayState::Connecting;
                s.latency_ms = None;
                ctx.request_repaint();
            }

            ui.add_space(8.0);

            // Settings
            if btn("⚙ Settings", "Open settings window").clicked() {
                // TODO: open settings window
            }
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn bold(text: &str) -> RichText {
    RichText::new(text).size(12.0).color(Color32::from_gray(160)).strong()
}

fn format_uptime(d: Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    if hours > 0 {
        format!("{}h {}m", hours, mins)
    } else {
        format!("{}m {}s", mins, secs)
    }
}

// Needed because we can't borrow NodeInfo.name as mut
trait StrExt {
    fn if_empty(&self, fallback: &str) -> &str;
}

impl StrExt for String {
    fn if_empty(&self, fallback: &str) -> &str {
        if self.is_empty() { fallback } else { self }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Builder helper
// ─────────────────────────────────────────────────────────────────────────────

/// Open the status window as a new egui viewport.
pub fn open_status_viewport(
    app: std::sync::Arc<tokio::sync::RwLock<SharedState>>,
) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Ozma Agent — Status")
            .with_inner_size([520.0, 400.0])
            .with_min_inner_size([400.0, 300.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Ozma Agent — Status",
        options,
        Box::new(|cc| Ok(Box::new(StatusWindow::new(cc, app)))),
    )
}
