use crate::config::{ActionType, AppConfig, ApprovalMode, Theme};
use egui::{ComboBox, Grid, RadioButton, Ui, Widget};

/// Connection test result
#[derive(Default, Clone)]
pub enum ConnectionStatus {
    #[default]
    NotTested,
    Testing,
    Success,
    Error(String),
}

/// Thread-local storage for connection test results (used by background thread)
mod connection_result {
    use std::sync::Mutex;
    
    struct TestResult {
        success: bool,
        error: Option<String>,
    }
    
    thread_local! {
        static RESULT: Mutex<Option<TestResult>> = Mutex::new(None);
    }
    
    pub fn set_success() {
        RESULT.with(|r| {
            *r.lock().unwrap() = Some(TestResult { success: true, error: None });
        });
    }
    
    pub fn set_error(msg: String) {
        RESULT.with(|r| {
            *r.lock().unwrap() = Some(TestResult { success: false, error: Some(msg) });
        });
    }
    
    pub fn take() -> Option<TestResult> {
        RESULT.with(|r| r.lock().unwrap().take())
    }
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
    /// Save operation result message
    save_message: Option<String>,
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
            save_message: None,
        }
    }

    /// Check if settings window is open
    pub fn is_open(&self) -> bool {
        self.config != self.working 
            || self.connection_status != ConnectionStatus::NotTested
            || self.save_message.is_some()
    }

    /// Show the settings window
    pub fn show(&mut self, ctx: &egui::Context, open: &mut bool) {
        if !*open {
            return;
        }

        // Poll for connection test result from background thread
        if let Some(result) = connection_result::take() {
            self.connection_status = if result.success {
                ConnectionStatus::Success
            } else {
                ConnectionStatus::Error(result.error.unwrap_or_else(|| "Unknown error".to_string()))
            };
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
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.working.agent_url)
                    .hint_text("http://localhost:7381")
                    .desired_width(300.0),
            );
            
            // Validate on change
            if response.changed() {
                self.is_dirty = true;
                self.save_message = None;
                // Clear error state when user types
                if let ConnectionStatus::Error(_) = &self.connection_status {
                    self.connection_status = ConnectionStatus::NotTested;
                }
            }
        });

        ui.add_space(8.0);

        ui.horizontal(|ui| {
            let is_testing = matches!(self.connection_status, ConnectionStatus::Testing);
            let button_enabled = !is_testing && !self.working.agent_url.trim().is_empty();
            
            if ui.add_enabled(button_enabled, egui::Button::new("Test Connection")).clicked() {
                self.test_connection();
            }

            match &self.connection_status {
                ConnectionStatus::NotTested => {
                    ui.label("").on_hover_text("Click 'Test Connection' to verify agent is reachable");
                }
                ConnectionStatus::Testing => {
                    ui.spinner();
                    ui.label("Testing...");
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
                    self.save_message = None;
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
                self.save_message = None;
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
                self.save_message = None;
            }

            if RadioButton::new(!self.working.notification_sound, "Off")
                .clicked()
            {
                self.working.notification_sound = false;
                self.is_dirty = true;
                self.save_message = None;
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
                self.save_message = None;
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
                self.save_message = None;
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
                self.save_message = None;
            }
        });
    }

    fn draw_footer(&mut self, ui: &mut Ui) {
        ui.separator();

        // Show save result message
        if let Some(msg) = &self.save_message {
            ui.colored_label(
                if msg.starts_with("✗") {
                    egui::Color32::RED
                } else {
                    egui::Color32::GREEN
                },
                msg,
            );
            ui.add_space(8.0);
        }

        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::RIGHT), |ui| {
                if ui.button("Cancel").clicked() {
                    self.cancel();
                }

                let has_changes = self.is_dirty || self.working != self.config;
                if ui
                    .button("Save")
                    .enabled(has_changes)
                    .clicked()
                {
                    self.save();
                }
            });
        });
    }

    fn test_connection(&mut self) {
        // Validate URL first before making HTTP request
        if let Err(e) = self.working.validate_agent_url() {
            self.connection_status = ConnectionStatus::Error(e);
            return;
        }

        let url = self.working.healthz_url();
        log::info!("Testing connection to {}", url);

        self.connection_status = ConnectionStatus::Testing;

        // Perform HTTP request in background thread to avoid blocking UI
        // Results are communicated back via thread-local storage
        let url_clone = url.clone();
        
        std::thread::spawn(move || {
            // Use ureq with timeouts to prevent hanging
            // 5s connect timeout, 5s read timeout
            let request = ureq::get(&url_clone)
                .timeout_connect(5_000)
                .timeout_read(5_000);
            
            match request.call() {
                Ok(response) if response.status() == 200 => {
                    log::info!("Connection test successful");
                    connection_result::set_success();
                }
                Ok(response) => {
                    let msg = format!("HTTP {}", response.status());
                    log::warn!("Connection test failed: {}", msg);
                    connection_result::set_error(msg);
                }
                Err(e) => {
                    let msg = e.to_string();
                    log::warn!("Connection test failed: {}", msg);
                    connection_result::set_error(msg);
                }
            }
        });
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

        // Validate URL before saving or making HTTP requests
        if let Err(e) = self.working.validate_agent_url() {
            self.save_message = Some(format!("✗ {}", e));
            return;
        }

        // Update autostart setting
        if let Err(e) = crate::autostart::set_launch_at_login(self.working.launch_at_login) {
            log::error!("Failed to set launch at login: {}", e);
            self.save_message = Some(format!("✗ Failed to update autostart: {}", e));
            return;
        }

        // Save config to disk
        if let Err(e) = self.working.save() {
            log::error!("Failed to save config: {}", e);
            self.save_message = Some(format!("✗ {}", e));
            return;
        }

        // Push approval modes to agent via HTTP
        self.push_approval_config();

        // Update our tracked config
        self.config = self.working.clone();
        self.is_dirty = false;

        self.save_message = Some("✓ Settings saved".to_string());
        log::info!("Settings saved successfully");
    }

    fn push_approval_config(&self) {
        // Build the payload from approval modes
        let approval_modes: std::collections::HashMap<String, String> = self
            .working
            .approval_modes
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.as_str().to_string()))
            .collect();

        let payload = serde_json::json!({
            "approval_modes": approval_modes
        });

        let url = self.working.config_push_url();
        log::info!("Pushing approval config to agent: {}", url);

        // Perform HTTP request in background thread
        // Using a simple fire-and-forget approach with error logging
        let url_clone = url.clone();
        let payload_clone = payload.clone();
        
        std::thread::spawn(move || {
            let request = ureq::put(&url_clone)
                .timeout_connect(5_000)
                .timeout_read(10_000)
                .set("Content-Type", "application/json");
            
            match request.send_json(&payload_clone) {
                Ok(response) => {
                    log::info!("Approval config pushed successfully: {}", response.status());
                }
                Err(e) => {
                    log::warn!("Failed to push approval config: {}", e);
                }
            }
        });
    }

    fn cancel(&mut self) {
        self.working = self.config.clone();
        self.is_dirty = false;
        self.connection_status = ConnectionStatus::NotTested;
        self.save_message = None;
        log::info!("Settings cancelled, reverted to saved state");
    }
}

impl ApprovalMode {
    fn as_str(&self) -> &'static str {
        match self {
            ApprovalMode::Auto => "Auto",
            ApprovalMode::Notify => "Notify",
            ApprovalMode::Approve => "Approve",
        }
    }
}
