use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Action types that can be configured for approval modes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

    pub fn as_str(&self) -> &'static str {
        match self {
            ActionType::ScreenCapture => "screen_capture",
            ActionType::KeyboardInput => "keyboard_input",
            ActionType::MouseClick => "mouse_click",
            ActionType::FileAccess => "file_access",
            ActionType::NetworkRequest => "network_request",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "screen_capture" => Some(ActionType::ScreenCapture),
            "keyboard_input" => Some(ActionType::KeyboardInput),
            "mouse_click" => Some(ActionType::MouseClick),
            "file_access" => Some(ActionType::FileAccess),
            "network_request" => Some(ActionType::NetworkRequest),
            _ => None,
        }
    }
}

/// How agent actions are handled
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalMode {
    Auto,
    Notify,
    Approve,
}

impl ApprovalMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApprovalMode::Auto => "auto",
            ApprovalMode::Notify => "notify",
            ApprovalMode::Approve => "approve",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "auto" => Some(ApprovalMode::Auto),
            "notify" => Some(ApprovalMode::Notify),
            "approve" => Some(ApprovalMode::Approve),
            _ => None,
        }
    }
}

impl Default for ApprovalMode {
    fn default() -> Self {
        ApprovalMode::Auto
    }
}

/// UI theme preference
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Main application configuration
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
    /// Get the platform-specific config file path using dirs::config_dir()
    pub fn config_path() -> PathBuf {
        let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        path.push("ozma");
        path.push("agent-ui.json");
        path
    }

    /// Load config from disk, returning default if missing or invalid
    pub fn load() -> Self {
        let path = Self::config_path();
        log::info!("Loading config from: {:?}", path);

        if !path.exists() {
            log::info!("Config file not found, using defaults");
            return Self::default();
        }

        match fs::read_to_string(&path) {
            Ok(contents) => {
                match serde_json::from_str(&contents) {
                    Ok(config) => {
                        log::info!("Config loaded successfully");
                        config
                    }
                    Err(e) => {
                        log::warn!("Failed to parse config: {}, using defaults", e);
                        Self::default()
                    }
                }
            }
            Err(e) => {
                log::warn!("Failed to read config: {}, using defaults", e);
                Self::default()
            }
        }
    }

    /// Save config to disk, creating directories if needed
    pub fn save(&self) -> Result<(), String> {
        let path = Self::config_path();
        
        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config directory: {}", e))?;
        }

        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize config: {}", e))?;

        fs::write(&path, json)
            .map_err(|e| format!("Failed to write config: {}", e))?;

        log::info!("Config saved to: {:?}", path);
        Ok(())
    }

    /// Validate the agent URL before making HTTP requests
    /// Returns Ok(()) if valid, Err(message) if invalid
    pub fn validate_agent_url(&self) -> Result<(), String> {
        let url = self.agent_url.trim();
        if url.is_empty() {
            return Err("Agent URL cannot be empty".to_string());
        }

        // Must be a valid HTTP(S) URL
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err("Agent URL must start with http:// or https://".to_string());
        }

        // Basic URL parsing check
        if url.parse::<url::Url>().is_err() {
            return Err("Agent URL is not a valid URL".to_string());
        }

        Ok(())
    }

    /// Get the full health check URL
    pub fn healthz_url(&self) -> String {
        let base = self.agent_url.trim_end_matches('/');
        format!("{}/healthz", base)
    }

    /// Get the agent config push URL
    pub fn config_push_url(&self) -> String {
        let base = self.agent_url.trim_end_matches('/');
        format!("{}/api/v1/agent/config", base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(config.agent_url, "http://localhost:7381");
        assert_eq!(config.approval_modes[&ActionType::ScreenCapture], ApprovalMode::Approve);
        assert_eq!(config.approval_modes[&ActionType::KeyboardInput], ApprovalMode::Notify);
        assert_eq!(config.approval_modes[&ActionType::NetworkRequest], ApprovalMode::Auto);
    }

    #[test]
    fn test_validate_agent_url() {
        let mut config = AppConfig::default();
        
        config.agent_url = "".to_string();
        assert!(config.validate_agent_url().is_err());
        
        config.agent_url = "invalid".to_string();
        assert!(config.validate_agent_url().is_err());
        
        config.agent_url = "http://localhost:7381".to_string();
        assert!(config.validate_agent_url().is_ok());
    }

    #[test]
    fn test_config_path() {
        let path = AppConfig::config_path();
        assert!(path.to_string_lossy().contains("ozma"));
        assert!(path.to_string_lossy().contains("agent-ui.json"));
    }
}
