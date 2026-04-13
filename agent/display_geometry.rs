// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Display geometry enumeration for Ozma Agent.
//!
//! Enumerates monitors via platform-specific APIs:
//! - Linux: winit + /sys/class/drm/*/edid
//! - Windows: winit + DXGI
//! - macOS: winit
//!
//! Reports to controller via REST API.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tokio::fs;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DisplayInfo {
    pub id: String,
    pub name: String,
    pub width_px: u32,
    pub height_px: u32,
    pub position_x: i32,
    pub position_y: i32,
    pub dpi: f64,
    pub refresh_rate: f64,
    pub physical_width_mm: f64,
    pub physical_height_mm: f64,
    pub edid: Option<String>, // base64 encoded
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DisplayReport {
    pub displays: Vec<DisplayInfo>,
    pub timestamp: f64,
}

/// Enumerate displays using winit
pub async fn enumerate_displays() -> Result<Vec<DisplayInfo>, Box<dyn std::error::Error>> {
    let mut displays = Vec::new();
    
    // Try to create an event loop to access monitors
    match create_display_enumerator().await {
        Ok(display_handles) => {
            for (index, handle) in display_handles.iter().enumerate() {
                let info = extract_display_info(handle, index).await?;
                displays.push(info);
            }
        }
        Err(e) => {
            eprintln!("Failed to enumerate displays with winit: {}", e);
        }
    }
    
    // Try to get additional EDID data on Linux
    #[cfg(target_os = "linux")]
    {
        let edid_data = read_linux_edid().await?;
        merge_edid_data(&mut displays, &edid_data);
    }
    
    Ok(displays)
}

async fn create_display_enumerator() -> Result<Vec<winit::monitor::MonitorHandle>, Box<dyn std::error::Error>> {
    use winit::event_loop::EventLoop;
    
    let event_loop = EventLoop::new()?;
    let mut displays = Vec::new();
    
    if let Some(primary) = event_loop.primary_monitor() {
        displays.push(primary);
    }
    
    displays.extend(event_loop.available_monitors());
    
    Ok(displays)
}

async fn extract_display_info(
    handle: &winit::monitor::MonitorHandle,
    index: usize,
) -> Result<DisplayInfo, Box<dyn std::error::Error>> {
    let name = handle.name().unwrap_or_else(|| format!("Display {}", index));
    let size = handle.size();
    let position = handle.position();
    
    // Get scale factor (DPI)
    let scale_factor = handle.scale_factor();
    let dpi = scale_factor * 96.0; // Assuming 96 DPI as base
    
    // Get refresh rate if available
    let refresh_rate = handle.refresh_rate().unwrap_or(60.0) as f64;
    
    // Physical size might be available
    let (physical_width_mm, physical_height_mm) = match handle.physical_size() {
        Some(size) => (size.width as f64, size.height as f64),
        None => (0.0, 0.0), // Will be filled from EDID
    };
    
    Ok(DisplayInfo {
        id: format!("display-{}", index),
        name,
        width_px: size.width,
        height_px: size.height,
        position_x: position.x,
        position_y: position.y,
        dpi,
        refresh_rate,
        physical_width_mm,
        physical_height_mm,
        edid: None, // Will be filled separately
    })
}

#[cfg(target_os = "linux")]
async fn read_linux_edid() -> Result<HashMap<String, Vec<u8>>, Box<dyn std::error::Error>> {
    use std::fs;
    
    let mut edid_map = HashMap::new();
    
    let drm_path = "/sys/class/drm";
    if let Ok(entries) = fs::read_dir(drm_path) {
        for entry in entries {
            if let Ok(entry) = entry {
                let edid_file = entry.path().join("edid");
                if edid_file.exists() {
                    if let Ok(edid_data) = fs::read(&edid_file) {
                        if edid_data.len() >= 23 {
                            // Use the directory name as key
                            if let Some(display_name) = entry.file_name().to_str() {
                                edid_map.insert(display_name.to_string(), edid_data);
                            }
                        }
                    }
                }
            }
        }
    }
    
    Ok(edid_map)
}

#[cfg(target_os = "linux")]
fn merge_edid_data(displays: &mut Vec<DisplayInfo>, edid_data: &HashMap<String, Vec<u8>>) {
    use base64::{Engine as _, engine::general_purpose};
    
    for (display_name, edid_bytes) in edid_data {
        // Try to find a matching display
        if let Some(display) = displays.iter_mut().find(|d| d.name.contains(display_name)) {
            // Parse physical dimensions from EDID
            if edid_bytes.len() >= 23 {
                let width_cm = edid_bytes[21] as f64;
                let height_cm = edid_bytes[22] as f64;
                display.physical_width_mm = width_cm * 10.0;
                display.physical_height_mm = height_cm * 10.0;
            }
            
            // Store base64 encoded EDID
            display.edid = Some(general_purpose::STANDARD.encode(edid_bytes));
        }
    }
}

#[cfg(target_os = "windows")]
async fn read_windows_edid() -> Result<HashMap<String, Vec<u8>>, Box<dyn std::error::Error>> {
    // Windows implementation would use DXGI to enumerate adapters
    // and read EDID data from the registry or direct adapter queries
    Ok(HashMap::new())
}

/// Report display geometry to the controller
pub async fn report_display_geometry(
    controller_url: &str,
    node_id: &str,
    displays: Vec<DisplayInfo>,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    
    let report = DisplayReport {
        displays,
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs_f64(),
    };
    
    let url = format!("{}/api/v1/nodes/{}/display-geometry", controller_url, node_id);
    
    let response = client
        .post(&url)
        .json(&report)
        .send()
        .await?;
    
    if !response.status().is_success() {
        return Err(format!("Controller returned error: {}", response.status()).into());
    }
    
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_enumerate_displays() {
        // This test will only work in environments with a display server
        match enumerate_displays().await {
            Ok(displays) => {
                // At least one display should be found in most environments
                println!("Found {} displays", displays.len());
                for display in displays {
                    println!("Display: {} - {}x{}", display.name, display.width_px, display.height_px);
                }
            }
            Err(e) => {
                // This is expected in headless environments
                eprintln!("Display enumeration failed (expected in headless): {}", e);
            }
        }
    }
}
