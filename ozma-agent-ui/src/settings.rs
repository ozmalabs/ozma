use crate::config::{ActionType, AppConfig, ApprovalMode, Theme};
use egui::{ComboBox, Grid, RadioButton, Ui, Widget};

/// Connection test result
#[derive(Default, Clone)]
pub enum ConnectionStatus {
    #[default]
    NotTested,
    Success,
    Error(String),
}

/// Settings window state
pub struct SettingsWindow {
    /// Current configuration (loaded from disk)
    config: AppConfig,
    /// Working copy that can be modified before saving
    working: AppConfig,
    /// Connection test status
    connection_status: ConnectionStatus,
    /// Whether changes have been made
    is_dirty: bool,
}

impl SettingsWindow {
    pub fn new() -> Self {
        let config = AppConfig::load();
        let working = config.clone();
        Self {
            config,
            working,
            connection_status: ConnectionStatus::NotTested,
            is_dirty: false,
        }
    }

    /// Check if settings window is open
    pub fn is_open(&self) -> bool {
        self.config != self.working || self.connection_status != ConnectionStatus::NotTested
    }

    /// Show the settings window
    pub fn show(&mut self, ctx: &egui::Context, open: &mut bool) {
        if !*open {
            return;
        }

        egui::Window::new("Settings")
            .open(open)
            .resizable(true)
            .min_width(500.0)
            .min_height(400.0)
            .default_width(550.0)
            .show(ctx, |ui| {
                ui.set_min_width(500.0);
                self.draw_contents(ui);
            });
    }

    fn draw_contents(&mut self, ui: &mut Ui) {
        ui.vertical(|ui| {
            // Section 1: Connection
            self.draw_connection_section(ui);

            ui.add_space(16.0);

            // Section 2: Privacy & Approvals
            self.draw_approvals_section(ui);

            ui.add_space(16.0);

            // Section 3: Startup
            self.draw_startup_section(ui);

            ui.add_space(16.0);

            // Section 4: Appearance
            self.draw_appearance_section(ui);

            ui.add_space(20.0);

            // Footer buttons
            self.draw_footer(ui);
        });
    }

    fn draw_connection_section(&mut self, ui: &mut Ui) {
        ui.heading("Connection");

        ui.horizontal(|ui| {
            ui.label("Agent URL:");
            ui.add(
                egui::TextEdit::singleline(&mut self.working.agent_url)
                    .hint_text("http://localhost:7381")
                    .desired_width(300.0),
            );
        });

        ui.add_space(8.0);

        ui.horizontal(|ui| {
            if ui.button("Test Connection").clicked() {
                self.test_connection();
            }

            match &self.connection_status {
                ConnectionStatus::NotTested => {
                    ui.label("").on_hover_text("Click 'Test Connection' to verify agent is reachable");
                }
                ConnectionStatus::Success => {
                    ui.label("✓ Connected").on_hover_text("Agent is reachable");
                }
                ConnectionStatus::Error(msg) => {
                    ui.label(format!("✗ {}", msg)).on_hover_text(msg.clone());
                }
            }
        });
    }

    fn draw_approvals_section(&mut self, ui: &mut Ui) {
        ui.heading("Privacy & Approvals");
        ui.label("Configure how each action type is handled by the agent.");

        ui.add_space(8.0);

        Grid::new("approval_grid").num_columns(2).spacing([20.0, 8.0]).show(ui, |ui| {
            for action in [
                ActionType::ScreenCapture,
                ActionType::KeyboardInput,
                ActionType::MouseClick,
                ActionType::FileAccess,
                ActionType::NetworkRequest,
            ] {
                ui.label(action.display_name());

                let current_mode = self
                    .working
                    .approval_modes
                    .get(&action)
                    .cloned()
                    .unwrap_or_default();

                let mut selected = current_mode;
                
                ComboBox::from_id_salt(format!("approval_{:?}", action))
                    .selected_text(selected.as_str())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut selected, ApprovalMode::Auto, "Auto");
                        ui.selectable_value(&mut selected, ApprovalMode::Notify, "Notify");
                        ui.selectable_value(&mut selected, ApprovalMode::Approve, "Approve");
                    });

                if selected != current_mode {
                    self.working.approval_modes.insert(action, selected);
                    self.is_dirty = true;
                }

                ui.end_row();
            }
        });
    }

    fn draw_startup_section(&mut self, ui: &mut Ui) {
        ui.heading("Startup");

        ui.horizontal(|ui| {
            if ui.checkbox(&mut self.working.launch_at_login, "Launch at login").clicked() {
                self.is_dirty = true;
            }
        });

        ui.add_space(8.0);

        ui.label("Notification sound:");

        ui.horizontal(|ui| {
            if RadioButton::new(self.working.notification_sound, "On")
                .clicked()
            {
                self.working.notification_sound = true;
                self.is_dirty = true;
            }

            if RadioButton::new(!self.working.notification_sound, "Off")
                .clicked()
            {
                self.working.notification_sound = false;
                self.is_dirty = true;
            }
        });
    }

    fn draw_appearance_section(&mut self, ui: &mut Ui) {
        ui.heading("Appearance");

        ui.label("Theme:");

        ui.horizontal(|ui| {
            if RadioButton::new(
                matches!(self.working.theme, Theme::System),
                "System",
            )
            .clicked()
            {
                self.working.theme = Theme::System;
                self.apply_theme(ui.ctx());
                self.is_dirty = true;
            }

            if RadioButton::new(
                matches!(self.working.theme, Theme::Light),
                "Light",
            )
            .clicked()
            {
                self.working.theme = Theme::Light;
                self.apply_theme(ui.ctx());
                self.is_dirty = true;
            }

            if RadioButton::new(
                matches!(self.working.theme, Theme::Dark),
                "Dark",
            )
            .clicked()
            {
                self.working.theme = Theme::Dark;
                self.apply_theme(ui.ctx());
                self.is_dirty = true;
            }
        });
    }

    fn draw_footer(&mut self, ui: &mut Ui) {
        ui.separator();

        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::RIGHT), |ui| {
                if ui.button("Cancel").clicked() {
                    self.cancel();
                }

                if ui
                    .button("Save")
                    .enabled(self.is_dirty || self.working != self.config)
                    .clicked()
                {
                    self.save();
                }
            });
        });
    }

    fn test_connection(&mut self) {
        let url = format!("{}/healthz", self.working.agent_url.trim_end_matches('/'));
        
        log::info!("Testing connection to {}", url);

        // Use a simple HTTP client to test the connection
        match ureq::get(&url).call() {
            Ok(response) => {
                if response.status() == 200 {
                    log::info!("Connection test successful");
                    self.connection_status = ConnectionStatus::Success;
                } else {
                    let msg = format!("HTTP {}", response.status());
                    log::warn!("Connection test failed: {}", msg);
                    self.connection_status = ConnectionStatus::Error(msg);
                }
            }
            Err(e) => {
                let msg = e.to_string();
                log::warn!("Connection test failed: {}", msg);
                self.connection_status = ConnectionStatus::Error(msg);
            }
        }
    }

    fn apply_theme(&self, ctx: &egui::Context) {
        match self.working.theme {
            Theme::System => {
                ctx.set_visuals(egui::Visuals::system());
            }
            Theme::Light => {
                ctx.set_visuals(egui::Visuals::light());
            }
            Theme::Dark => {
                ctx.set_visuals(egui::Visuals::dark());
            }
        }
    }

    fn save(&mut self) {
        log::info!("Saving settings...");

        // Update autostart setting
        if let Err(e) = crate::autostart::set_launch_at_login(self.working.launch_at_login) {
            log::error!("Failed to set launch at login: {}", e);
        }

        // Save config to disk
        if let Err(e) = self.working.save() {
            log::error!("Failed to save config: {}", e);
            return;
        }

        // Push approval modes to agent
        self.push_approval_config();

        // Update our tracked config
        self.config = self.working.clone();
        self.is_dirty = false;

        log::info!("Settings saved successfully");
    }

    fn push_approval_config(&self) {
        let approval_modes: std::collections::HashMap<String, String> = self
            .working
            .approval_modes
            .iter()
            .map(|(k, v)| (format!("{:?}", k).to_lowercase().replace("request", "request"), v.as_str().to_lowercase()))
            .collect();

        let payload = serde_json::json!({
            "approval_modes": approval_modes
        });

        let url = format!("{}/api/v1/agent/config", self.working.agent_url.trim_end_matches('/'));
        
        log::info!("Pushing approval config to agent: {}", url);

        match ureq::put(&url)
            .send_json(&payload)
        {
            Ok(response) => {
                log::info!("Approval config pushed successfully: {}", response.status());
            }
            Err(e) => {
                log::warn!("Failed to push approval config: {}", e);
            }
        }
    }

    fn cancel(&mut self) {
        self.working = self.config.clone();
        self.is_dirty = false;
        // Reset connection test status
        self.connection_status = ConnectionStatus::NotTested;
        log::info!("Settings cancelled, reverted to saved state");
    }
}

impl ApprovalMode {
    fn as_str(&self) -> &'static str {
        match self {
            ApprovalMode::Auto => "auto",
            ApprovalMode::Notify => "notify",
            ApprovalMode::Approve => "approve",
        }
    }
}
