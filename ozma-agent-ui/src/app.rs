use eframe::egui;
use tracing::info;

pub struct OzmaApp {
    agent_url: String,
}

impl OzmaApp {
    pub fn new(agent_url: String) -> Self {
        Self { agent_url }
    }
}

impl eframe::App for OzmaApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Ozma Agent Manager");
            ui.label(format!("Connecting to: {}", self.agent_url));
            
            ui.separator();
            
            ui.label("Status: Disconnected (Stub)");
            
            if ui.button("Connect").clicked() {
                info!("Connect button clicked (stub)");
            }
        });
    }
}
