// src/ui/transport.rs — панель транспорта: GO, STOP, PAUSE и мастер-громкость

use crate::config::AppearanceSettings;
use crate::performance::PerformanceSnapshot;
use crate::ui::icons::IconManager;
use crate::ui::theme::Theme;
use egui::*;

pub enum TransportAction {
    Go,
    StopAll,
    PauseAll,
}

pub struct TransportBar;

fn contrasting_icon_color(background: Color32) -> Color32 {
    let luminance = 0.2126 * background.r() as f32
        + 0.7152 * background.g() as f32
        + 0.0722 * background.b() as f32;
    if luminance >= 145.0 {
        Color32::BLACK
    } else {
        Color32::WHITE
    }
}

fn paint_transport_icon(
    ui: &Ui,
    button_rect: Rect,
    texture_id: TextureId,
    center: Pos2,
    size: f32,
) {
    let icon_rect = Rect::from_center_size(center, Vec2::splat(size));
    ui.painter().image(
        texture_id,
        icon_rect.intersect(button_rect),
        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
        Color32::WHITE,
    );
}

fn output_level_db(peak: f32) -> Option<f32> {
    if !peak.is_finite() || peak <= 0.000_001 {
        None
    } else {
        Some(20.0 * peak.log10())
    }
}

fn paint_output_meter(ui: &mut Ui, peak: f32) {
    let db = output_level_db(peak);
    let value_text = db.map_or_else(|| "−∞ dBFS".to_string(), |value| format!("{value:.1} dBFS"));
    let value_color = match db {
        Some(value) if value >= -3.0 => Color32::from_rgb(235, 75, 65),
        Some(value) if value >= -12.0 => Color32::from_rgb(230, 180, 65),
        Some(_) => Color32::from_rgb(85, 190, 125),
        None => Theme::text_dim(),
    };

    // Reserve a fixed-width value column so changing dB digits cannot move
    // neighbouring transport controls or resize the meter section.
    ui.allocate_ui_with_layout(
        vec2(196.0, 16.0),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.add_sized(
                [82.0, 15.0],
                Label::new(
                    RichText::new("OUTPUT PEAK")
                        .color(Theme::text_dim())
                        .size(Theme::scaled_font_size(10.0)),
                ),
            );
            ui.add_space(4.0);
            ui.add_sized(
                [106.0, 15.0],
                Label::new(
                    RichText::new(value_text)
                        .color(value_color)
                        .size(Theme::scaled_font_size(10.0))
                        .monospace(),
                )
                .halign(Align::RIGHT),
            );
        },
    );

    let (bar_rect, response) = ui.allocate_exact_size(vec2(196.0, 13.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(bar_rect, 3_u8, Theme::bg_dark());

    let ratio = db.map_or(0.0, |value| ((value + 60.0) / 60.0).clamp(0.0, 1.0));
    let fill_width = bar_rect.width() * ratio;
    if fill_width > 0.0 {
        let fill_color = match db {
            Some(value) if value >= -3.0 => Color32::from_rgb(235, 75, 65),
            Some(value) if value >= -12.0 => Color32::from_rgb(230, 180, 65),
            _ => Color32::from_rgb(85, 190, 125),
        };
        let fill_rect = Rect::from_min_max(
            bar_rect.left_top(),
            Pos2::new(bar_rect.left() + fill_width, bar_rect.bottom()),
        );
        painter.rect_filled(fill_rect, 3_u8, fill_color);
    }

    // Fixed dB reference marks: -36, -18, -6 and 0 dBFS.
    for mark_db in [-36.0_f32, -18.0, -6.0, 0.0] {
        let x = bar_rect.left() + ((mark_db + 60.0) / 60.0) * bar_rect.width();
        painter.line_segment(
            [
                Pos2::new(x, bar_rect.top()),
                Pos2::new(x, bar_rect.bottom()),
            ],
            Stroke::new(1.0_f32, Color32::from_white_alpha(105)),
        );
    }

    response.on_hover_text(
        "Sample peak of the summed digital mix on each output endpoint, after Cue routing and volume. The display shows the highest peak among endpoints; separate physical outputs are not summed. Display scale: −60 to 0 dBFS; over-range values are shown numerically in red. Not a hardware loopback measurement.",
    );
}

fn paint_cpu_indicator(ui: &mut Ui, performance: &PerformanceSnapshot) {
    let system_cpu = performance
        .system_cpu
        .current
        .map(|value| value.clamp(0.0, 100.0));
    let app_cpu = performance.app_cpu.current;
    let app_memory = performance.app_memory_mb.current;
    let worker_cpu = performance.audio_worker_cpu.current;
    let color = match system_cpu {
        Some(value) if value >= 90.0 => Color32::from_rgb(235, 75, 65),
        Some(value) if value >= 70.0 => Color32::from_rgb(230, 180, 65),
        Some(_) => Theme::text_active(),
        None => Theme::text_dim(),
    };

    ui.vertical(|ui| {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("CPU")
                    .color(Theme::text_dim())
                    .size(Theme::scaled_font_size(10.0)),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(system_cpu.map_or_else(|| "--%".to_string(), |v| format!("{v:.0}%")))
                    .color(color)
                    .size(Theme::scaled_font_size(11.0)),
            );
        });

        let (bar_rect, response) = ui.allocate_exact_size(vec2(64.0, 7.0), Sense::hover());
        ui.painter().rect_filled(bar_rect, 2_u8, Theme::bg_dark());
        if let Some(value) = system_cpu {
            let fill_rect = Rect::from_min_max(
                bar_rect.left_top(),
                Pos2::new(
                    bar_rect.left() + bar_rect.width() * (value as f32 / 100.0_f32),
                    bar_rect.bottom(),
                ),
            );
            ui.painter().rect_filled(fill_rect, 2_u8, color);
        }
        response.on_hover_text("System-wide CPU load, sampled once per second");

        ui.label(
            RichText::new(format!(
                "APP {}% · {} MB",
                app_cpu.map_or_else(|| "--".to_string(), |value| format!("{value:.0}")),
                app_memory.map_or_else(|| "--".to_string(), |value| format!("{value:.0}"))
            ))
            .color(Theme::text_dim())
            .size(Theme::scaled_font_size(9.0)),
        );
        ui.label(
            RichText::new(format!(
                "AUDIO {} · {}%",
                performance.active_audio_workers,
                worker_cpu.map_or_else(|| "--".to_string(), |value| format!("{value:.0}"))
            ))
            .color(Theme::text_dim())
            .size(Theme::scaled_font_size(9.0)),
        );
    });
}

impl TransportBar {
    pub fn show(
        ui: &mut Ui,
        icons: &mut IconManager,
        appearance: &AppearanceSettings,
        active_count: usize,
        total_count: usize,
        output_peak: f32,
        performance: &PerformanceSnapshot,
        master_vol: &mut u8,
        bottom_panel_visible: &mut bool,
    ) -> Option<TransportAction> {
        let mut action = None;
        let icon_context = ui.ctx().clone();
        let go_fill = Theme::go_btn();
        let stop_fill = Theme::stop_btn();
        let pause_fill = Theme::pause_btn();
        let go_icon = icons.texture_id_for_color(
            &icon_context,
            "play",
            appearance,
            contrasting_icon_color(go_fill),
        );
        let stop_icon = icons.texture_id_for_color(
            &icon_context,
            "stop",
            appearance,
            contrasting_icon_color(stop_fill),
        );
        let pause_icon = icons.texture_id_for_color(
            &icon_context,
            "pause",
            appearance,
            contrasting_icon_color(pause_fill),
        );
        ui.horizontal(|ui| {
            ui.add_space(4.0);

            // GO keeps its original green button and dimensions; only the
            // play glyph is replaced with the matching SVG.
            let go_resp = ui.add_sized(
                [80.0, 44.0],
                Button::new("").fill(go_fill).corner_radius(6_u8),
            );
            if let Some(texture_id) = go_icon {
                paint_transport_icon(
                    ui,
                    go_resp.rect,
                    texture_id,
                    Pos2::new(go_resp.rect.left() + 19.0, go_resp.rect.center().y),
                    20.0,
                );
            }
            ui.painter().text(
                Pos2::new(go_resp.rect.left() + 54.0, go_resp.rect.center().y),
                Align2::CENTER_CENTER,
                "GO",
                FontId::proportional(Theme::scaled_font_size(15.0)),
                Color32::WHITE,
            );
            if go_resp.clicked() {
                action = Some(TransportAction::Go);
            }
            go_resp.on_hover_text("GO / Resume selected Cue or Group (Space)");

            ui.add_space(8.0);

            // STOP button keeps its existing size and appearance, but now stops all cues.
            let stop_resp = ui.add_sized(
                [60.0, 44.0],
                Button::new("").fill(stop_fill).corner_radius(6_u8),
            );
            if let Some(texture_id) = stop_icon {
                paint_transport_icon(
                    ui,
                    stop_resp.rect,
                    texture_id,
                    stop_resp.rect.center(),
                    24.0,
                );
            }
            if stop_resp.on_hover_text("Stop all cues (Escape)").clicked() {
                action = Some(TransportAction::StopAll);
            }

            // PAUSE button keeps its existing size and appearance, but pauses all cues.
            let pause_resp = ui.add_sized(
                [60.0, 44.0],
                Button::new("").fill(pause_fill).corner_radius(6_u8),
            );
            if let Some(texture_id) = pause_icon {
                paint_transport_icon(
                    ui,
                    pause_resp.rect,
                    texture_id,
                    pause_resp.rect.center(),
                    24.0,
                );
            }
            if pause_resp
                .on_hover_text("Pause all cues (Shift+P)")
                .clicked()
            {
                action = Some(TransportAction::PauseAll);
            }

            ui.add_space(12.0);
            ui.add(Separator::default().vertical());
            ui.add_space(8.0);

            // Two-line Cue status: active playback count and total project Cue count.
            ui.vertical(|ui| {
                ui.add_space(3.0);
                let active_color = if active_count > 0 {
                    Theme::text_active()
                } else {
                    Theme::text_dim()
                };
                ui.label(
                    RichText::new(format!("{active_count} active cues"))
                        .color(active_color)
                        .size(Theme::scaled_font_size(12.0)),
                );
                ui.label(
                    RichText::new(format!("{total_count} cues"))
                        .color(Theme::text_dim())
                        .size(Theme::scaled_font_size(10.0)),
                );
            });

            // Мастер-громкость
            ui.add_space(12.0);
            ui.add(Separator::default().vertical());
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.add_space(2.0);
                ui.label(
                    RichText::new("MASTER")
                        .color(Theme::text_dim())
                        .size(Theme::scaled_font_size(10.0)),
                );
                ui.add(
                    Slider::new(master_vol, 0_u8..=100_u8)
                        .show_value(true)
                        .suffix("%")
                        .text(""),
                );
            });

            // Live output peak meter, sampled in the audio workers and forwarded at 10 Hz.
            ui.add_space(12.0);
            ui.add(Separator::default().vertical());
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.add_space(2.0);
                paint_output_meter(ui, output_peak);
            });

            // System CPU usage is sampled once per second outside the audio thread.
            ui.add_space(12.0);
            ui.add(Separator::default().vertical());
            ui.add_space(8.0);
            paint_cpu_indicator(ui, performance);

            // Keep the lower-panel visibility control in one stable location:
            // at the far right of the complete transport bar, after the monitors.
            ui.add_space(12.0);
            ui.add(Separator::default().vertical());
            ui.add_space(8.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (label, tooltip) = if *bottom_panel_visible {
                    ("Hide panel ▾", "Hide lower panel")
                } else {
                    ("Show panel ▴", "Show lower panel")
                };
                if ui
                    .add_sized(
                        [116.0, 44.0],
                        Button::new(RichText::new(label).size(Theme::scaled_font_size(12.0)))
                            .fill(Theme::button_bg())
                            .corner_radius(6_u8),
                    )
                    .on_hover_text(tooltip)
                    .clicked()
                {
                    *bottom_panel_visible = !*bottom_panel_visible;
                }
            });
        });

        action
    }
}
