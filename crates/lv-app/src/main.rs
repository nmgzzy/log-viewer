#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod fmt;
mod fonts;
mod i18n;
mod tab;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([800.0, 480.0])
            .with_title("Log Viewer"),
        ..Default::default()
    };
    eframe::run_native(
        "uf-log-viewer",
        options,
        Box::new(|cc| Ok(Box::new(app::LogViewerApp::new(cc)))),
    )
}
