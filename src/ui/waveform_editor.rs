// src/ui/waveform_editor.rs — редактор вейвформы + управление Region & Loop

use crate::cue::Cue;
use crate::ui::theme::Theme;
use egui::*;

pub struct WaveformEditor;

impl WaveformEditor {
    pub fn show(ui: &mut Ui, cue: &mut Cue) {
        let dur = cue.duration_secs.max(0.01);
        let eff_end = cue.effective_end_secs();

        // Панель управления Region & Loop над вейвформой
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Region & Loop:").color(Theme::ACCENT).strong());
            ui.separator();

            ui.label("Start:");
            ui.add(
                DragValue::new(&mut cue.start_secs)
                    .range(0.0..=eff_end)
                    .speed(0.02)
                    .suffix(" s"),
            );

            let mut has_custom_end = cue.end_secs.is_some();
            if ui.checkbox(&mut has_custom_end, "Custom end point").changed() {
                cue.end_secs = if has_custom_end {
                    Some(cue.duration_secs)
                } else {
                    None
                };
            }

            let start_min = cue.start_secs;
            if let Some(ref mut end_val) = cue.end_secs {
                ui.add(
                    DragValue::new(end_val)
                        .range(start_min..=dur)
                        .speed(0.02)
                        .suffix(" s"),
                );
            }

            ui.separator();

            let current_start = cue.start_secs;
            let current_eff_end = cue.effective_end_secs();
            if ui
                .checkbox(&mut cue.loop_region.enabled, "Enable Loop")
                .changed()
                && cue.loop_region.enabled
                && cue.loop_region.end_secs <= cue.loop_region.start_secs
            {
                cue.loop_region.start_secs = current_start;
                cue.loop_region.end_secs = current_eff_end;
            }

            if cue.loop_region.enabled {
                let loop_end_max = cue.loop_region.end_secs.max(0.01);
                ui.label("Loop Start:");
                ui.add(
                    DragValue::new(&mut cue.loop_region.start_secs)
                        .range(0.0..=loop_end_max)
                        .speed(0.02)
                        .suffix(" s"),
                );

                let loop_start_min = cue.loop_region.start_secs;
                ui.label("Loop End:");
                ui.add(
                    DragValue::new(&mut cue.loop_region.end_secs)
                        .range(loop_start_min..=dur)
                        .speed(0.02)
                        .suffix(" s"),
                );

                let mut inf = cue.loop_region.count < 0;
                if ui.checkbox(&mut inf, "∞ Infinite").changed() {
                    cue.loop_region.count = if inf { -1 } else { 2 };
                }
                if !inf {
                    let mut n = cue.loop_region.count.max(1);
                    if ui
                        .add(DragValue::new(&mut n).range(1..=999).suffix("×"))
                        .changed()
                    {
                        cue.loop_region.count = n;
                    }
                }
            }
        });

        ui.add_space(4.0);

        let height = (ui.available_height() - 4.0).max(95.0);
        let (rect, resp) =
            ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click_and_drag());

        let painter = ui.painter_at(rect);

        painter.rect_filled(rect, 4.0_f32, Color32::from_rgb(14, 14, 20));
        painter.rect_stroke(
            rect,
            4.0_f32,
            Stroke::new(1.0_f32, Theme::BORDER),
            egui::StrokeKind::Outside,
        );

        if cue.waveform.is_empty() || cue.duration_secs <= 0.0 {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "No waveform data (select an Audio cue with a loaded file)",
                FontId::proportional(12.0),
                Theme::TEXT_DIM,
            );
            return;
        }

        let secs_to_x = |secs: f64| -> f32 {
            rect.left() + (secs / dur).clamp(0.0, 1.0) as f32 * rect.width()
        };
        let x_to_secs = |x: f32| -> f64 {
            (((x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64) * dur
        };

        let eff_end = cue.effective_end_secs();

        // Отрисовка пиков вейвформы
        let buckets = cue.waveform.len() / 2;
        let bucket_w = rect.width() / buckets.max(1) as f32;
        let mid_y = rect.center().y;
        let half_h = rect.height() * 0.44;

        for i in 0..buckets {
            let min_val = cue.waveform[i * 2];
            let max_val = cue.waveform[i * 2 + 1];
            let x = rect.left() + i as f32 * bucket_w;
            let y_top = mid_y + min_val * half_h;
            let y_bot = mid_y + max_val * half_h;

            let secs_at_bucket = (i as f64 / buckets as f64) * dur;
            let outside_region = secs_at_bucket < cue.start_secs || secs_at_bucket > eff_end;
            let in_loop = cue.loop_region.enabled
                && secs_at_bucket >= cue.loop_region.start_secs
                && secs_at_bucket <= cue.loop_region.end_secs;

            let color = if outside_region {
                Color32::from_rgba_unmultiplied(70, 70, 85, 90)
            } else if in_loop {
                Color32::from_rgba_premultiplied(80, 160, 255, 190)
            } else {
                Theme::WAVE_FILL
            };

            painter.line_segment(
                [
                    pos2(x + bucket_w * 0.5, y_top),
                    pos2(x + bucket_w * 0.5, y_bot),
                ],
                Stroke::new(bucket_w.max(1.0_f32), color),
            );
        }

        // Линии Fade In и Fade Out
        if cue.fade_in_secs > 0.0 {
            let x0 = secs_to_x(cue.start_secs);
            let x1 = secs_to_x((cue.start_secs + cue.fade_in_secs).min(eff_end));
            painter.line_segment(
                [pos2(x0, rect.bottom() - 14.0), pos2(x1, rect.top() + 8.0)],
                Stroke::new(2.0_f32, Theme::ACCENT2),
            );
        }
        if cue.fade_out_secs > 0.0 {
            let x0 = secs_to_x((eff_end - cue.fade_out_secs).max(cue.start_secs));
            let x1 = secs_to_x(eff_end);
            painter.line_segment(
                [pos2(x0, rect.top() + 8.0), pos2(x1, rect.bottom() - 14.0)],
                Stroke::new(2.0_f32, Theme::ACCENT),
            );
        }

        // Центральная ось
        painter.line_segment(
            [pos2(rect.left(), mid_y), pos2(rect.right(), mid_y)],
            Stroke::new(0.5_f32, Theme::BORDER),
        );

        // Курсор воспроизведения
        if cue.is_active() && cue.position_secs > 0.0 {
            let px = secs_to_x(cue.position_secs);
            painter.line_segment(
                [pos2(px, rect.top()), pos2(px, rect.bottom())],
                Stroke::new(2.0_f32, Color32::WHITE),
            );
        }

        // Маркеры Start / End / Loop
        Self::draw_marker(
            &painter,
            secs_to_x(cue.start_secs),
            rect,
            Theme::MARKER_START,
            "▼ START",
        );

        if cue.end_secs.is_some() {
            Self::draw_marker(
                &painter,
                secs_to_x(eff_end),
                rect,
                Theme::MARKER_END,
                "▼ END",
            );
        }

        if cue.loop_region.enabled {
            Self::draw_marker(
                &painter,
                secs_to_x(cue.loop_region.start_secs),
                rect,
                Theme::MARKER_LOOP,
                "▼ L-IN",
            );
            Self::draw_marker(
                &painter,
                secs_to_x(cue.loop_region.end_secs),
                rect,
                Theme::MARKER_LOOP,
                "▼ L-OUT",
            );
        }

        // Шкала времени
        let tick_count = 10_usize;
        for i in 0..=tick_count {
            let t = dur * i as f64 / tick_count as f64;
            let x = secs_to_x(t);
            painter.line_segment(
                [pos2(x, rect.bottom() - 8.0), pos2(x, rect.bottom())],
                Stroke::new(1.0_f32, Theme::BORDER),
            );
            painter.text(
                pos2(x, rect.bottom() - 9.0),
                Align2::CENTER_BOTTOM,
                format!("{:.1}s", t),
                FontId::monospace(8.5),
                Theme::TEXT_DIM,
            );
        }

        // Перетаскивание маркеров мышью
        if resp.dragged() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let secs = x_to_secs(pos.x).clamp(0.0, dur);
                let start_dx = (secs_to_x(cue.start_secs) - pos.x).abs();
                let end_dx = cue
                    .end_secs
                    .map(|e| (secs_to_x(e) - pos.x).abs())
                    .unwrap_or(f32::INFINITY);
                let loop_s_dx = if cue.loop_region.enabled {
                    (secs_to_x(cue.loop_region.start_secs) - pos.x).abs()
                } else {
                    f32::INFINITY
                };
                let loop_e_dx = if cue.loop_region.enabled {
                    (secs_to_x(cue.loop_region.end_secs) - pos.x).abs()
                } else {
                    f32::INFINITY
                } ;

                let min_dx = start_dx.min(end_dx).min(loop_s_dx).min(loop_e_dx);
                if min_dx < 25.0 {
                    if min_dx == start_dx {
                        cue.start_secs = secs.min(eff_end);
                    } else if min_dx == end_dx {
                        cue.end_secs = Some(secs.max(cue.start_secs));
                    } else if min_dx == loop_s_dx {
                        cue.loop_region.start_secs = secs.min(cue.loop_region.end_secs);
                    } else {
                        cue.loop_region.end_secs = secs.max(cue.loop_region.start_secs);
                    }
                }
            }
        }
    }

    fn draw_marker(painter: &Painter, x: f32, rect: Rect, color: Color32, label: &str) {
        painter.line_segment(
            [pos2(x, rect.top()), pos2(x, rect.bottom() - 12.0)],
            Stroke::new(1.5_f32, color),
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