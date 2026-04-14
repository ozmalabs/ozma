use eframe::egui;
use tracing::info;

use crate::client::OzmaClient;
use crate::tray::TrayStatus;

/// Main application state for the Ozma Agent Manager window
pub struct OzmaApp {
    /// URL of the agent to connect to
    agent_url: String,
    /// HTTP client for agent communication
    client: OzmaClient,
    /// Current connection status
    status: TrayStatus,
}

impl OzmaApp {
    /// Create a new application instance
    pub fn new(agent_url: String) -> Self {
        Self {
            agent_url: agent_url.clone(),
            client: OzmaClient::new(agent_url),
            status: TrayStatus::Disconnected,
        }
    }

    /// Get the current agent URL
    pub fn agent_url(&self) -> &str {
        &self.agent_url
    }

    /// Get the current status
    pub fn status(&self) -> TrayStatus {
        self.status
    }

    /// Set the connection status
    pub fn set_status(&mut self, status: TrayStatus) {
        self.status = status;
    }
}

impl eframe::App for OzmaApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Ozma Agent Manager");
            ui.label(format!("Agent URL: {}", self.agent_url));
            
            ui.separator();
            
            // Display current status
            let status_text = match self.status {
                TrayStatus::Disconnected => "Status: Disconnected",
                TrayStatus::Connecting => "Status: Connecting...",
                TrayStatus::Connected => "Status: Connected",
                TrayStatus::Error => "Status: Error",
            };
            ui.label(status_text);
            
            ui.separator();
            
            // Connect button
            if ui.button("Connect").clicked() {
                info!("Connect button clicked - would connect to agent");
                self.status = TrayStatus::Connecting;
            }
            
            // Disconnect button
            if ui.button("Disconnect").clicked() {
                info!("Disconnect button clicked");
                self.status = TrayStatus::Disconnected;
            }
            
            ui.separator();
            
            // Quit button
            if ui.button("Quit").clicked() {
                info!("Quit button clicked - closing window");
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}
