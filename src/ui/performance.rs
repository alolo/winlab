use crate::performance::{DispatchStatsSnapshot, MetricSnapshot, PerformanceSnapshot};
use crate::ui::theme::Theme;
use egui::{self, Grid, RichText, Ui};

fn metric_text(metric: MetricSnapshot, unit: &str) -> String {
    match metric.current {
        Some(value) => format!("{value:.1}{unit}"),
        None => "—".to_string(),
    }
}

fn stats_text(metric: MetricSnapshot, unit: &str) -> String {
    match (metric.average, metric.p95, metric.maximum) {
        (Some(average), Some(p95), Some(maximum)) => {
            format!(
                "avg {average:.1}{unit} · P95 {p95:.1}{unit} · max {maximum:.1}{unit} · samples {}",
                metric.samples
            )
        }
        _ => "Collecting samples…".to_string(),
    }
}

fn dispatch_section(ui: &mut Ui, title: &str, stats: DispatchStatsSnapshot) {
    ui.heading(title);
    Grid::new(format!("performance_{title}_grid"))
        .num_columns(2)
        .spacing([18.0, 5.0])
        .striped(true)
        .show(ui, |ui| {
            ui.label("Batches");
            ui.label(stats.batches.to_string());
            ui.end_row();

            ui.label("Events");
            ui.label(stats.events.to_string());
            ui.end_row();

            ui.label("Accepted / sent");
            ui.label(stats.sent.to_string());
            ui.end_row();

            ui.label("Failed");
            ui.label(stats.failed.to_string());
            ui.end_row();

            ui.label("Skipped");
            ui.label(stats.skipped.to_string());
            ui.end_row();

            ui.label("Queue wait · avg / max");
            ui.label(format!(
                "{} / {} µs",
                stats
                    .average_queue_wait_us
                    .map_or_else(|| "—".to_string(), |value| format!("{value:.1}")),
                stats
                    .maximum_queue_wait_us
                    .map_or_else(|| "—".to_string(), |value| value.to_string())
            ));
            ui.end_row();

            ui.label("Worker time · avg / max");
            ui.label(format!(
                "{} / {} µs",
                stats
                    .average_worker_elapsed_us
                    .map_or_else(|| "—".to_string(), |value| format!("{value:.1}")),
                stats
                    .maximum_worker_elapsed_us
                    .map_or_else(|| "—".to_string(), |value| value.to_string())
            ));
            ui.end_row();
        });
}

pub fn show(ui: &mut Ui, snapshot: &PerformanceSnapshot) {
    ui.heading("Performance Monitor");
    ui.label(
        RichText::new(format!(
            "Session: {} min {} sec · sampling interval: 1 second",
            snapshot.session_elapsed_secs / 60,
            snapshot.session_elapsed_secs % 60
        ))
        .color(Theme::text_dim())
        .size(Theme::scaled_font_size(10.5)),
    );
    ui.separator();

    ui.heading("System and process");
    Grid::new("performance_system_grid")
        .num_columns(3)
        .spacing([22.0, 8.0])
        .striped(true)
        .show(ui, |ui| {
            ui.label(RichText::new("Metric").strong());
            ui.label(RichText::new("Current").strong());
            ui.label(RichText::new("Session avg / P95 / max").strong());
            ui.end_row();

            ui.label("System CPU");
            ui.label(metric_text(snapshot.system_cpu, "%"));
            ui.label(stats_text(snapshot.system_cpu, "%"));
            ui.end_row();

            ui.label("TaSQ CPU");
            ui.label(metric_text(snapshot.app_cpu, "%"));
            ui.label(stats_text(snapshot.app_cpu, "%"));
            ui.end_row();

            ui.label("TaSQ resident RAM");
            ui.label(metric_text(snapshot.app_memory_mb, " MB"));
            ui.label(stats_text(snapshot.app_memory_mb, " MB"));
            ui.end_row();

            ui.label("System RAM load");
            ui.label(metric_text(snapshot.system_memory_percent, "%"));
            ui.label(stats_text(snapshot.system_memory_percent, "%"));
            ui.end_row();

            ui.label("Audio Worker CPU (aggregate)");
            ui.label(metric_text(snapshot.audio_worker_cpu, "%"));
            ui.label(stats_text(snapshot.audio_worker_cpu, "%"));
            ui.end_row();

            ui.label("Audio Worker RAM (aggregate)");
            ui.label(metric_text(snapshot.audio_worker_memory_mb, " MB"));
            ui.label(stats_text(snapshot.audio_worker_memory_mb, " MB"));
            ui.end_row();
        });

    ui.add_space(10.0);
    ui.heading("GUI responsiveness");
    Grid::new("performance_gui_grid")
        .num_columns(2)
        .spacing([18.0, 5.0])
        .striped(true)
        .show(ui, |ui| {
            ui.label("Measured GUI updates");
            ui.label(snapshot.gui_update.frames.to_string());
            ui.end_row();

            ui.label("Average update time");
            ui.label(
                snapshot
                    .gui_update
                    .average_ms
                    .map_or_else(|| "—".to_string(), |value| format!("{value:.2} ms")),
            );
            ui.end_row();

            ui.label("P95 update time");
            ui.label(snapshot.gui_update.p95_ms.map_or_else(
                || "—".to_string(),
                |value| format!("{value:.1} ms (histogram estimate)"),
            ));
            ui.end_row();

            ui.label("Maximum update time");
            ui.label(
                snapshot
                    .gui_update
                    .maximum_ms
                    .map_or_else(|| "—".to_string(), |value| format!("{value:.2} ms")),
            );
            ui.end_row();

            ui.label("Updates over 33 ms");
            ui.label(snapshot.gui_update.over_33ms.to_string());
            ui.end_row();
        });

    ui.add_space(10.0);
    ui.heading("Audio Worker health");
    Grid::new("performance_audio_worker_grid")
        .num_columns(2)
        .spacing([18.0, 5.0])
        .striped(true)
        .show(ui, |ui| {
            ui.label("Active endpoint Workers");
            ui.label(snapshot.active_audio_workers.to_string());
            ui.end_row();

            ui.label("Maximum heartbeat age");
            ui.label(
                snapshot
                    .worker_heartbeat_max_age_ms
                    .map_or_else(|| "—".to_string(), |value| format!("{value} ms")),
            );
            ui.end_row();

            ui.label("Endpoint losses");
            ui.label(snapshot.audio_worker_losses.to_string());
            ui.end_row();

            ui.label("Successful recoveries");
            ui.label(snapshot.audio_worker_recoveries.to_string());
            ui.end_row();

            ui.label("Recovery failures");
            ui.label(snapshot.audio_worker_recovery_failures.to_string());
            ui.end_row();
        });

    ui.add_space(10.0);
    ui.columns(2, |columns| {
        dispatch_section(&mut columns[0], "MIDI dispatch", snapshot.midi);
        dispatch_section(&mut columns[1], "OSC dispatch", snapshot.osc);
    });

    ui.add_space(10.0);
    ui.label(
        RichText::new(
            "Audio underrun count is not available through the current cross-backend audio API. Native stream failures, Worker losses and recovery events are tracked separately. MIDI sent counts mean accepted by the MIDI backend; OSC sent counts mean accepted by the local UDP stack, not acknowledged by the receiver. Process CPU percentages are normalized to total logical CPU capacity (100% means all logical processors fully used). CPU/RAM sampling does not poll audio drivers and runs outside the audio render callback.",
        )
        .color(Theme::text_dim())
        .size(Theme::scaled_font_size(10.5)),
    );
}
