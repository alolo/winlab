// Waveform editor — visual waveform with draggable markers

use crate::cue::AudioCue;
use crate::ui::theme::Theme;
use egui::*;

pub struct WaveformEditor;

impl WaveformEditor {
    pub fn show(ui: &mut Ui, cue: &mut AudioCue) {
        let height = 120.0;
        let (rect, resp) = ui.allocate_exact_size(
            vec2(ui.available_width(), height),
            Sense::click_and_drag(),
        );

        let painter = ui.painter_at(rect);

        // Background
        painter.rect_filled(rect, 4.0, Color32::from_rgb(14, 14, 20));
        painter.rect_stroke(rect, 4.0, Stroke::new(1.0_f32, Theme::BORDER), egui::StrokeKind::Outside);

        if cue.waveform.is_empty() || cue.duration_secs <= 0.0 {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "No waveform data",
                FontId::proportional(12.0),
                Theme::TEXT_DIM,
            );
            return;
        }

        let dur = cue.duration_secs;
        let secs_to_x = |secs: f64| -> f32 {
            rect.left() + (secs / dur) as f32 * rect.width()
        };
        let x_to_secs = |x: f32| -> f64 {
            ((x - rect.left()) / rect.width()) as f64 * dur
        };

        // Draw waveform peaks
        let buckets = cue.waveform.len() / 2;
        let bucket_w = rect.width() / buckets as f32;
        let mid_y = rect.center().y;
        let half_h = rect.height() * 0.45;

        for i in 0..buckets {
            let min_val = cue.waveform[i * 2];
            let max_val = cue.waveform[i * 2 + 1];
            let x = rect.left() + i as f32 * bucket_w;
            let y_top = mid_y + min_val * half_h;
            let y_bot = mid_y + max_val * half_h;

            // Loop region background
            let secs_at_bucket = (i as f64 / buckets as f64) * dur;
            let in_loop = cue.loop_region.enabled
                && secs_at_bucket >= cue.loop_region.start_secs
                && secs_at_bucket <= cue.loop_region.end_secs;

            let color = if in_loop {
                Color32::from_rgba_premultiplied(80, 160, 255, 180)
            } else {
                Theme::WAVE_FILL
            };

            painter.line_segment(
                [pos2(x + bucket_w * 0.5, y_top), pos2(x + bucket_w * 0.5, y_bot)],
                Stroke::new(bucket_w.max(1.0), color),
            );
        }

        // Centre line
        painter.line_segment(
            [pos2(rect.left(), mid_y), pos2(rect.right(), mid_y)],
            Stroke::new(0.5, Theme::BORDER),
        );

        // Playback position
        if cue.is_active() && cue.position_secs > 0.0 {
            let px = secs_to_x(cue.position_secs);
            painter.line_segment(
                [pos2(px, rect.top()), pos2(px, rect.bottom())],
                Stroke::new(2.0, Color32::WHITE),
            );
        }

        // START marker (green triangle)
        Self::draw_marker(
            &painter,
            secs_to_x(cue.start_secs),
            rect,
            Theme::MARKER_START,
            "▼ S",
        );

        // LOOP markers
        if cue.loop_region.enabled {
            Self::draw_marker(
                &painter,
                secs_to_x(cue.loop_region.start_secs),
                rect,
                Theme::MARKER_LOOP,
                "▼ L",
            );
            Self::draw_marker(
                &painter,
                secs_to_x(cue.loop_region.end_secs),
                rect,
                Theme::MARKER_END,
                "▼ E",
            );
        }

        // Time ruler at bottom
        let tick_count = 10_usize;
        for i in 0..=tick_count {
            let t = dur * i as f64 / tick_count as f64;
            let x = secs_to_x(t);
            let label = format!("{:.1}s", t);
            painter.line_segment(
                [pos2(x, rect.bottom() - 8.0), pos2(x, rect.bottom())],
                Stroke::new(1.0, Theme::BORDER),
            );
            painter.text(
                pos2(x, rect.bottom() - 10.0),
                Align2::CENTER_BOTTOM,
                label,
                FontId::monospace(8.0),
                Theme::TEXT_DIM,
            );
        }

        // Drag handling — move START marker
        if resp.dragged() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let secs = x_to_secs(pos.x).clamp(0.0, dur);
                // Determine closest marker to snap to
                let start_dx = (secs_to_x(cue.start_secs) - pos.x).abs();
                let loop_s_dx = if cue.loop_region.enabled {
                    (secs_to_x(cue.loop_region.start_secs) - pos.x).abs()
                } else {
                    f32::INFINITY
                };
                let loop_e_dx = if cue.loop_region.enabled {
                    (secs_to_x(cue.loop_region.end_secs) - pos.x).abs()
                } else {
                    f32::INFINITY
                };

                if start_dx <= loop_s_dx && start_dx <= loop_e_dx && start_dx < 20.0 {
                    cue.start_secs = secs;
                } else if loop_s_dx <= loop_e_dx && loop_s_dx < 20.0 {
                    cue.loop_region.start_secs = secs.min(cue.loop_region.end_secs);
                } else if loop_e_dx < 20.0 {
                    cue.loop_region.end_secs = secs.max(cue.loop_region.start_secs);
                }
            }
        }
    }

    fn draw_marker(painter: &Painter, x: f32, rect: Rect, color: Color32, label: &str) {
        painter.line_segment(
            [pos2(x, rect.top()), pos2(x, rect.bottom() - 12.0)],
            Stroke::new(1.5, color),
        );
        painter.text(
            pos2(x, rect.top() + 2.0),
            Align2::CENTER_TOP,
            label,
            FontId::monospace(9.0),
            color,
        );
    }
}
