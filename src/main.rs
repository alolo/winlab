// CueLabs — Professional Audio Cue Player
// Entry point

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use tracing_subscriber::EnvFilter;

mod app;
mod audio;
mod config;
mod cue;
mod export;
mod midi;
mod osc;
mod project;
mod ui;

fn main() -> anyhow::Result<()> {
    // Logging
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("cuelabs=debug,warn"))
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("CueLabs")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([900.0, 600.0])
            .with_drag_and_drop(true)
            .with_icon(eframe::icon_data::from_png_bytes(&[]).unwrap_or_default()),
        ..Default::default()
    };

    eframe::run_native(
        "CueLabs",
        options,
        Box::new(|cc| Ok(Box::new(app::CueLabsApp::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}
