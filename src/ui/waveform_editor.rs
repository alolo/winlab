// src/ui/waveform_editor.rs — редактор вейвформы + управление Region & Loop

use crate::cue::{Cue, CueState};
use crate::ui::theme::Theme;
use egui::*;

pub struct WaveformEditor;

#[derive(Debug, Clone, Copy)]
struct WaveformViewState {
    zoom: f32,
    offset: f32,
}

impl Default for WaveformViewState {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            offset: 0.0,
        }
    }
}

impl WaveformEditor {
    pub fn show(ui: &mut Ui, cue: &mut Cue) -> Option<f64> {
        let dur = cue.duration_secs.max(0.01);
        let eff_end = cue.effective_end_secs();

        // Панель управления Region & Loop над вейвформой
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new("Region & Loop:")
                    .color(Theme::ACCENT)
                    .strong(),
            );
            ui.separator();

            ui.label("Start:");
            ui.add(
                DragValue::new(&mut cue.start_secs)
                    .range(0.0..=eff_end)
                    .speed(0.02)
                    .suffix(" s"),
            );

            let mut has_custom_end = cue.end_secs.is_some();
            if ui
                .checkbox(&mut has_custom_end, "Custom end point")
                .changed()
            {
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
                if ui.checkbox(&mut inf, "в€ћ Infinite").changed() {
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

        let view_id = Id::new(("waveform_view_state", cue.id));
        let mut view = ui
            .ctx()
            .data(|data| data.get_temp::<WaveformViewState>(view_id))
            .unwrap_or_default();
        let scrollbar_height = if view.zoom > 1.01 { 20.0 } else { 0.0 };
        let height = (ui.available_height() - 4.0 - scrollbar_height).max(95.0);
        let (rect, resp) =
            ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click_and_drag());

        // Alt + mouse wheel zooms around the pointer position. Shift + wheel
        // pans horizontally through the zoomed waveform.
        let (scroll_delta, modifiers, pointer_pos) = ui.ctx().input(|input| {
            (
                input.smooth_scroll_delta,
                input.modifiers,
                input.pointer.hover_pos(),
            )
        });
        if let Some(pointer) = pointer_pos.filter(|position| rect.contains(*position)) {
            if modifiers.alt && scroll_delta.y.abs() > f32::EPSILON {
                let old_span = 1.0 / view.zoom.max(1.0);
                let pointer_fraction =
                    ((pointer.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
                let anchored_time = view.offset + pointer_fraction * old_span;
                view.zoom = (view.zoom * (scroll_delta.y * 0.003).exp()).clamp(1.0, 1_000_000.0);
                let new_span = 1.0 / view.zoom;
                view.offset =
                    (anchored_time - pointer_fraction * new_span).clamp(0.0, 1.0 - new_span);
                // Do not let the bottom panel's parent ScrollArea consume the
                // same wheel gesture after the waveform has handled zoom.
                ui.ctx().input_mut(|input| {
                    input.raw_scroll_delta = Vec2::ZERO;
                    input.smooth_scroll_delta = Vec2::ZERO;
                });
            } else if modifiers.shift {
                // Some platforms report Shift+wheel as horizontal delta; others
                // keep it in y. Accept either representation.
                let pan_delta = if scroll_delta.x.abs() > f32::EPSILON {
                    scroll_delta.x
                } else {
                    scroll_delta.y
                };
                if pan_delta.abs() > f32::EPSILON {
                    let span = 1.0 / view.zoom.max(1.0);
                    view.offset =
                        (view.offset - pan_delta * 0.002 / view.zoom).clamp(0.0, 1.0 - span);
                    ui.ctx().input_mut(|input| {
                        input.raw_scroll_delta = Vec2::ZERO;
                        input.smooth_scroll_delta = Vec2::ZERO;
                    });
                }
            }
        }
        ui.ctx().data_mut(|data| data.insert_temp(view_id, view));

        let view_span = 1.0 / view.zoom.max(1.0);
        let view_start = view.offset.clamp(0.0, 1.0 - view_span);
        let view_end = (view_start + view_span).min(1.0);

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
            return None;
        }

        let secs_to_x = |secs: f64| -> f32 {
            let normalized = (secs / dur) as f32;
            rect.left() + ((normalized - view_start) / view_span) * rect.width()
        };
        let x_to_secs = |x: f32| -> f64 {
            let fraction = ((x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
            ((view_start + fraction * view_span) as f64 * dur).clamp(0.0, dur)
        };

        let eff_end = cue.effective_end_secs();

        // Draw only the visible peak buckets. This keeps zooming smooth even
        // for long files with dense waveform data.
        let buckets = cue.waveform.len() / 2;
        let first_bucket = ((view_start * buckets as f32).floor() as usize).min(buckets);
        let last_bucket = ((view_end * buckets as f32).ceil() as usize).min(buckets);
        let visible_buckets = last_bucket.saturating_sub(first_bucket).max(1);
        let bucket_w = rect.width() / visible_buckets as f32;
        let mid_y = rect.center().y;
        let half_h = rect.height() * 0.44;

        for i in first_bucket..last_bucket {
            let min_val = cue.waveform[i * 2];
            let max_val = cue.waveform[i * 2 + 1];
            let secs_at_bucket = (i as f64 / buckets.max(1) as f64) * dur;
            let x = secs_to_x(secs_at_bucket + (dur / buckets.max(1) as f64) * 0.5);
            let y_top = mid_y + min_val * half_h;
            let y_bot = mid_y + max_val * half_h;
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

        // Adaptive time ruler. A 60-minute file uses 2-minute ticks at the
        // widest view; zooming in automatically moves to finer intervals.
        let visible_duration = (dur * view_span as f64).max(0.001);
        let target_tick_count = (rect.width() / 132.0).clamp(4.0, 24.0) as f64;
        let raw_step = visible_duration / target_tick_count;
        let nice_steps = [
            0.001_f64, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0,
            30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 1800.0, 3600.0,
        ];
        let tick_step = if dur >= 3599.0 && view.zoom <= 1.01 {
            120.0
        } else {
            nice_steps
                .iter()
                .copied()
                .find(|step| *step >= raw_step)
                .unwrap_or(3600.0)
        };
        let visible_start_secs = view_start as f64 * dur;
        let visible_end_secs = view_end as f64 * dur;
        let mut tick = (visible_start_secs / tick_step).floor() * tick_step;
        if tick < visible_start_secs {
            tick += tick_step;
        }
        let mut tick_guard = 0usize;
        while tick <= visible_end_secs + tick_step * 0.0001 && tick_guard < 500 {
            let x = secs_to_x(tick);
            if x >= rect.left() - 1.0 && x <= rect.right() + 1.0 {
                painter.line_segment(
                    [pos2(x, rect.bottom() - 13.0), pos2(x, rect.bottom())],
                    Stroke::new(1.0_f32, Theme::BORDER),
                );
                painter.text(
                    pos2(x, rect.bottom() - 13.0),
                    Align2::CENTER_BOTTOM,
                    format_timecode(tick),
                    FontId::monospace(10.0),
                    Color32::WHITE,
                );
            }
            tick += tick_step;
            tick_guard += 1;
        }

        painter.text(
            pos2(rect.right() - 8.0, rect.top() + 6.0),
            Align2::RIGHT_TOP,
            format!("{:.1}×", view.zoom),
            FontId::monospace(12.0),
            Color32::WHITE,
        );

        // A plain click sets the playhead and pauses the cue at that position.
        // The app forwards the returned position to the audio engine; if the cue
        // is currently stopped, the next trigger starts from this saved position.
        let mut seek_position = None;
        if resp.clicked() {
            if let Some(pointer_pos) = resp.interact_pointer_pos() {
                let position = x_to_secs(pointer_pos.x).clamp(cue.start_secs, eff_end);
                cue.position_secs = position;
                cue.state = CueState::Paused;
                seek_position = Some(position);
            }
        }

        // Dragging near a marker edits the marker instead of seeking.
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
                };

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

        // Full-width waveform scrollbar. The track spans the whole editor and
        // keeps a rounded thumb; dragging or clicking it pans without changing zoom.
        if view.zoom > 1.01 {
            ui.add_space(2.0);
            let width = ui.available_width().max(40.0);
            let (scroll_rect, scroll_response) =
                ui.allocate_exact_size(vec2(width, 18.0), Sense::click_and_drag());
            let painter = ui.painter_at(scroll_rect);
            let track = Rect::from_min_max(
                pos2(scroll_rect.left() + 4.0, scroll_rect.center().y - 3.0),
                pos2(scroll_rect.right() - 4.0, scroll_rect.center().y + 3.0),
            );
            painter.rect_filled(track, 6.0, Color32::from_rgb(42, 44, 54));
            let span = (1.0 / view.zoom.max(1.0)).clamp(0.000_001, 1.0);
            let max_offset = (1.0 - span).max(0.0);
            let thumb_width = (track.width() * span).clamp(24.0, track.width());
            let thumb_travel = (track.width() - thumb_width).max(0.0);
            let thumb_left = track.left()
                + if max_offset > 0.0 {
                    (view.offset / max_offset).clamp(0.0, 1.0) * thumb_travel
                } else {
                    0.0
                };
            let thumb_rect = Rect::from_min_size(
                pos2(thumb_left, scroll_rect.center().y - 7.0),
                vec2(thumb_width, 14.0),
            );
            painter.rect_filled(thumb_rect, 7.0, Theme::ACCENT);
            painter.circle_filled(thumb_rect.center(), 4.0, Color32::WHITE);

            if (scroll_response.dragged() || scroll_response.clicked()) && max_offset > 0.0 {
                if let Some(pointer) = scroll_response.interact_pointer_pos() {
                    let centered_left = pointer.x - track.left() - thumb_width * 0.5;
                    view.offset =
                        (centered_left / thumb_travel.max(1.0)).clamp(0.0, 1.0) * max_offset;
                    ui.ctx().request_repaint();
                }
            }
        }
        ui.ctx().data_mut(|data| data.insert_temp(view_id, view));

        seek_position
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

fn format_timecode(seconds: f64) -> String {
    let total_ms = (seconds.max(0.0) * 1000.0).round() as u64;
    let hours = total_ms / 3_600_000;
    let minutes = (total_ms / 60_000) % 60;
    let whole_seconds = (total_ms / 1_000) % 60;
    let millis = total_ms % 1_000;
    format!("{hours:02}:{minutes:02}:{whole_seconds:02}.{millis:03}")
}

#[cfg(test)]
mod timecode_tests {
    use super::format_timecode;

    #[test]
    fn formats_hours_minutes_seconds_and_milliseconds() {
        assert_eq!(format_timecode(3661.234), "01:01:01.234");
        assert_eq!(format_timecode(120.0), "00:02:00.000");
    }
}
