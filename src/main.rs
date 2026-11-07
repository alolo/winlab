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
    // A short-lived helper mode used to query native ASIO channel names.
    // It runs before tracing/GUI initialization and writes its result to a
    // temporary JSON file consumed by the parent CueLabs process.
    #[cfg(all(target_os = "windows", feature = "asio"))]
    {
        let args: Vec<String> = std::env::args().collect();
        if args.get(1).map(String::as_str) == Some("--cuelabs-asio-probe") {
            let driver_index = args
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("Missing ASIO driver index"))?
                .parse::<usize>()?;
            let output_path = args
                .get(3)
                .ok_or_else(|| anyhow::anyhow!("Missing ASIO probe output path"))?;
            return audio::asio_probe::run_probe_child(
                driver_index,
                std::path::Path::new(output_path),
            );
        }
    }

    // Logging
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("cuelabs=debug,warn"))
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("CueLabs")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([900.0, 600.0])
            .with_drag_and_drop(true) // ← перенесли сюда
            .with_icon(eframe::icon_data::from_png_bytes(&[]).unwrap_or_default()),
        ..Default::default()
    };

    tracing::info!("Starting CueLabs native window");
    let result = eframe::run_native(
        "CueLabs",
        options,
        Box::new(|cc| Ok(Box::new(app::CueLabsApp::new(cc)))),
    );
    tracing::info!("CueLabs native event loop returned");
    result.map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}
