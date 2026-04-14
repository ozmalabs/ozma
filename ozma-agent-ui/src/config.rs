use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Approval mode for agent actions
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ApprovalMode {
    Auto,
    Notify,
    Approve,
}

impl Default for ApprovalMode {
    fn default() -> Self {
        ApprovalMode::Notify
    }
}

/// Theme selection
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Theme {
    System,
    Light,
    Dark,
}

impl Default for Theme {
    fn default() -> Self {
        Theme::System
    }
}

/// Action types that can be controlled by approval modes
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ActionType {
    ScreenCapture,
    KeyboardInput,
    MouseClick,
    FileAccess,
    NetworkRequest,
}

impl ActionType {
    pub fn display_name(&self) -> &'static str {
        match self {
            ActionType::ScreenCapture => "Screen Capture",
            ActionType::KeyboardInput => "Keyboard Input",
            ActionType::MouseClick => "Mouse Click",
            ActionType::FileAccess => "File Access",
            ActionType::NetworkRequest => "Network Requests",
        }
    }
}

/// Application configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub agent_url: String,
    pub approval_modes: HashMap<ActionType, ApprovalMode>,
    pub launch_at_login: bool,
    pub theme: Theme,
    pub notification_sound: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        let mut approval_modes = HashMap::new();
        approval_modes.insert(ActionType::ScreenCapture, ApprovalMode::Approve);
        approval_modes.insert(ActionType::KeyboardInput, ApprovalMode::Notify);
        approval_modes.insert(ActionType::MouseClick, ApprovalMode::Notify);
        approval_modes.insert(ActionType::FileAccess, ApprovalMode::Approve);
        approval_modes.insert(ActionType::NetworkRequest, ApprovalMode::Auto);

        Self {
            agent_url: "http://localhost:7381".to_string(),
            approval_modes,
            launch_at_login: false,
            theme: Theme::System,
            notification_sound: true,
        }
    }
}

impl AppConfig {
    /// Returns the config file path using platform config directory
    pub fn config_path() -> PathBuf {
        let config_dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        config_dir.join("ozma").join("agent-ui.json")
    }

    /// Load configuration from disk
    pub fn load() -> Self {
        let path = Self::config_path();
        log::info!("Loading config from {:?}", path);

        match fs::read_to_string(&path) {
            Ok(contents) => {
                match serde_json::from_str(&contents) {
                    Ok(config) => {
                        log::info!("Config loaded successfully");
                        config
                    }
                    Err(e) => {
                        log::warn!("Failed to parse config, using defaults: {}", e);
                        Self::default()
                    }
                }
            }
            Err(e) => {
                log::info!("No config file found ({}), using defaults", e);
                Self::default()
            }
        }
    }

    /// Save configuration to disk
    pub fn save(&self) -> Result<(), String> {
        let path = Self::config_path();
        
        // Create parent directories if they don't exist
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config directory: {}", e))?;
        }

        // Write config with pretty formatting
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize config: {}", e))?;

        fs::write(&path, json)
            .map_err(|e| format!("Failed to write config file: {}", e))?;

        log::info!("Config saved to {:?}", path);
        Ok(())
    }

    /// Get approval mode for an action type
    pub fn get_approval_mode(&self, action: &ActionType) -> ApprovalMode {
        self.approval_modes
            .get(action)
            .cloned()
            .unwrap_or_default()
    }

    /// Set approval mode for an action type
    pub fn set_approval_mode(&mut self, action: ActionType, mode: ApprovalMode) {
        self.approval_modes.insert(action, mode);
    }
}
