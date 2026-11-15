// Theatre Show Cue — Professional Audio Cue Player
// Entry point

#![cfg_attr(all(not(debug_assertions), not(test)), windows_subsystem = "windows")]

use eframe::egui;
use tracing_subscriber::EnvFilter;

mod app;
mod audio;
mod config;
mod cue;
mod diagnostics;
mod export;
mod midi;
mod osc;
mod performance;
mod project;
mod ui;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // The audio worker is a child mode on every supported target. It runs
    // before GUI/logging initialization and reserves stdout for JSON-lines IPC.
    if args.get(1).map(String::as_str) == Some("--tasq-audio-worker") {
        return audio::worker::run_worker_child_stdio();
    }

    // A short-lived Windows helper mode used to query native ASIO channel names.
    // It writes its result to a temporary JSON file consumed by the parent process.
    #[cfg(target_os = "windows")]
    {
        if args.get(1).map(String::as_str) == Some("--tasq-asio-probe") {
            let driver_index = args
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("Missing ASIO driver index"))?
                .parse::<usize>()?;
            let output_path = args
                .get(3)
                .ok_or_else(|| anyhow::anyhow!("Missing ASIO probe output path"))?;
            let output_path = std::path::Path::new(output_path);
            let result = audio::asio_probe::run_probe_child(driver_index, output_path);
            if let Err(error) = &result {
                let diagnostic_path = output_path.with_extension("error.txt");
                let _ = std::fs::write(diagnostic_path, format!("{error:#}"));
            }
            return result;
        }
    }

    // Load settings before GUI creation so the diagnostic writers use the
    // user's selected folder from the very first startup event.
    let settings = config::AppSettings::load();
    if let Err(error) = diagnostics::init_logging(&settings) {
        eprintln!("Could not initialize diagnostic file logging: {error:#}");
        let _ = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("TaSQ=debug,TheatreShowCue=debug,warn"))
            .try_init();
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Theatre Show Cue")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([900.0, 600.0])
            .with_drag_and_drop(true) // ← перенесли сюда
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/TaSQ.png"))
                    .expect("embedded TaSQ window icon must be a valid PNG"),
            ),
        ..Default::default()
    };

    tracing::info!("Starting Theatre Show Cue native window");
    let result = eframe::run_native(
        "Theatre Show Cue",
        options,
        Box::new(|cc| Ok(Box::new(app::TheatreShowCueApp::new(cc)))),
    );
    tracing::info!("Theatre Show Cue native event loop returned");
    result.map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}
