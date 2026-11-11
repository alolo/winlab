// src/ui/waveform_editor.rs — редактор вейвформы + управление Region & Loop

use crate::cue::{Cue, CueState};
use crate::ui::theme::Theme;
use egui::*;

pub struct WaveformEditor;

fn snap_speed_value(value: f64) -> f64 {
    ((value * 10.0).round() / 10.0).clamp(0.1, 4.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum WaveformAutomationLane {
    Volume,
    Speed,
    Pitch,
}

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
    pub fn show(ui: &mut Ui, cue: &mut Cue) -> (Option<f64>, bool, bool) {
        let dur = cue.duration_secs.max(0.01);
        let eff_end = cue.effective_end_secs();
        let mut automation_changed = false;
        let mut time_pitch_changed = false;

        // Панель управления Region & Loop над вейвформой
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new("Region & Loop:")
                    .color(Theme::accent())
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

        let lane_id = Id::new(("waveform_automation_lane", cue.id));
        let mut automation_lane = ui
            .ctx()
            .data(|data| data.get_temp::<WaveformAutomationLane>(lane_id))
            .unwrap_or(WaveformAutomationLane::Volume);
        let (
            mut lane_enabled,
            mut automation_points,
            mut value_min,
            mut value_max,
            mut default_value,
        ) = match automation_lane {
            WaveformAutomationLane::Volume => (
                cue.volume_automation_enabled,
                cue.volume_automation.clone(),
                0.0_f64,
                1.0_f64,
                1.0_f64,
            ),
            WaveformAutomationLane::Speed => (
                cue.time_pitch.speed_automation_enabled,
                cue.time_pitch.speed_automation.clone(),
                0.1_f64,
                4.0_f64,
                1.0_f64,
            ),
            WaveformAutomationLane::Pitch => (
                cue.time_pitch.pitch_automation_enabled,
                cue.time_pitch.pitch_automation.clone(),
                -2400.0_f64,
                2400.0_f64,
                0.0_f64,
            ),
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Automation:").color(Theme::accent()).strong());
            ComboBox::from_id_salt(("waveform_automation_select", cue.id))
                .selected_text(match automation_lane {
                    WaveformAutomationLane::Volume => "Volume",
                    WaveformAutomationLane::Speed => "Playback speed (×)",
                    WaveformAutomationLane::Pitch => "Pitch shift (cents)",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut automation_lane, WaveformAutomationLane::Volume, "Volume");
                    ui.selectable_value(&mut automation_lane, WaveformAutomationLane::Speed, "Playback speed (×)");
                    ui.selectable_value(&mut automation_lane, WaveformAutomationLane::Pitch, "Pitch shift (cents)");
                });
            if automation_lane != ui
                .ctx()
                .data(|data| data.get_temp::<WaveformAutomationLane>(lane_id))
                .unwrap_or(WaveformAutomationLane::Volume)
            {
                ui.ctx().data_mut(|data| data.insert_temp(lane_id, automation_lane));
                let (enabled, points, min_value, max_value, default) = match automation_lane {
                    WaveformAutomationLane::Volume => (cue.volume_automation_enabled, cue.volume_automation.clone(), 0.0, 1.0, 1.0),
                    WaveformAutomationLane::Speed => (cue.time_pitch.speed_automation_enabled, cue.time_pitch.speed_automation.clone(), 0.1, 4.0, 1.0),
                    WaveformAutomationLane::Pitch => (cue.time_pitch.pitch_automation_enabled, cue.time_pitch.pitch_automation.clone(), -2400.0, 2400.0, 0.0),
                };
                lane_enabled = enabled;
                automation_points = points;
                value_min = min_value;
                value_max = max_value;
                default_value = default;
            }
            if ui.checkbox(&mut lane_enabled, "Enable").changed() {
                match automation_lane {
                    WaveformAutomationLane::Volume => automation_changed = true,
                    _ => time_pitch_changed = true,
                }
            }
            match automation_lane {
                WaveformAutomationLane::Speed => {
                    if ui
                        .checkbox(
                            &mut cue.time_pitch.preserve_pitch_on_speed_change,
                            "Preserve pitch (time-stretch)",
                        )
                        .on_hover_text(
                            "On: speed automation changes duration but keeps pitch. Off: pitch follows speed (varispeed).",
                        )
                        .changed()
                    {
                        time_pitch_changed = true;
                    }
                }
                WaveformAutomationLane::Pitch => {
                    if ui
                        .checkbox(
                            &mut cue.time_pitch.pitch_shift_changes_speed,
                            "Varispeed: pitch shift changes duration",
                        )
                        .on_hover_text(
                            "On: pitch automation also changes playback speed and duration. Off: pitch changes without changing duration.",
                        )
                        .changed()
                    {
                        time_pitch_changed = true;
                    }
                }
                WaveformAutomationLane::Volume => {}
            }
            if lane_enabled {
                let hint = match automation_lane {
                    WaveformAutomationLane::Volume => "0–100%",
                    WaveformAutomationLane::Speed => {
                        if cue.time_pitch.preserve_pitch_on_speed_change {
                            "0.1×–4.0× · pitch preserved"
                        } else {
                            "0.1×–4.0× · pitch follows speed"
                        }
                    }
                    WaveformAutomationLane::Pitch => {
                        if cue.time_pitch.pitch_shift_changes_speed {
                            "cents · speed follows pitch"
                        } else {
                            "cents · speed unchanged"
                        }
                    }
                };
                ui.label(RichText::new(format!("{} · click curve to add · drag points · right-click to remove", hint))
                    .color(Theme::text_dim()).size(10.0));
                if ui.small_button("Reset curve").clicked() {
                    automation_points = vec![[0.0, default_value], [dur, default_value]];
                    match automation_lane {
                        WaveformAutomationLane::Volume => automation_changed = true,
                        _ => time_pitch_changed = true,
                    }
                }
            }
        });

        if lane_enabled {
            let before = automation_points.clone();
            automation_points.retain(|point| point[0].is_finite() && point[1].is_finite());
            automation_points.sort_by(|a, b| a[0].total_cmp(&b[0]));
            for point in &mut automation_points {
                point[0] = point[0].clamp(0.0, dur);
                point[1] = point[1].clamp(value_min, value_max);
                if automation_lane == WaveformAutomationLane::Speed {
                    point[1] = snap_speed_value(point[1]);
                }
            }
            if automation_points.is_empty() {
                automation_points = vec![[0.0, default_value], [dur, default_value]];
            } else {
                if automation_points[0][0] > 0.0 {
                    let value = automation_points[0][1];
                    automation_points.insert(0, [0.0, value]);
                }
                let last_index = automation_points.len() - 1;
                if automation_points[last_index][0] < dur {
                    let value = automation_points[last_index][1];
                    automation_points.push([dur, value]);
                }
            }
            if before != automation_points {
                match automation_lane {
                    WaveformAutomationLane::Volume => automation_changed = true,
                    _ => time_pitch_changed = true,
                }
            }
        }

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

        painter.rect_filled(rect, 4.0_f32, Theme::bg_dark());
        painter.rect_stroke(
            rect,
            4.0_f32,
            Stroke::new(1.0_f32, Theme::border()),
            egui::StrokeKind::Outside,
        );

        if cue.waveform.is_empty() || cue.duration_secs <= 0.0 {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "No waveform data (select an Audio cue with a loaded file)",
                FontId::proportional(12.0),
                Theme::text_dim(),
            );
            Self::store_automation_lane(cue, automation_lane, lane_enabled, automation_points);
            return (None, automation_changed, time_pitch_changed);
        }

        let secs_to_x = |secs: f64| -> f32 {
            let normalized = (secs / dur) as f32;
            rect.left() + ((normalized - view_start) / view_span) * rect.width()
        };
        let x_to_secs = |x: f32| -> f64 {
            let fraction = ((x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
            ((view_start + fraction * view_span) as f64 * dur).clamp(0.0, dur)
        };
        let value_to_y = |value: f64| -> f32 {
            let usable_height = (rect.height() - 36.0).max(1.0);
            let normalized = ((value - value_min) / (value_max - value_min).max(0.000_001))
                .clamp(0.0, 1.0) as f32;
            rect.bottom() - 18.0 - normalized * usable_height
        };
        let y_to_value = |y: f32| -> f64 {
            let usable_height = (rect.height() - 36.0).max(1.0);
            let normalized = ((rect.bottom() - 18.0 - y) / usable_height).clamp(0.0, 1.0) as f64;
            value_min + normalized * (value_max - value_min)
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

        // A subtle but unmistakable fill distinguishes the active loop region
        // from the rest of the clip without obscuring waveform detail.
        if cue.loop_region.enabled {
            let loop_color = Theme::loop_region();
            let loop_left = secs_to_x(cue.loop_region.start_secs).clamp(rect.left(), rect.right());
            let loop_right = secs_to_x(cue.loop_region.end_secs).clamp(rect.left(), rect.right());
            if loop_right > loop_left {
                let loop_rect = Rect::from_min_max(
                    pos2(loop_left, rect.top()),
                    pos2(loop_right, rect.bottom()),
                );
                painter.rect_filled(
                    loop_rect,
                    0.0,
                    Color32::from_rgba_unmultiplied(
                        loop_color.r(),
                        loop_color.g(),
                        loop_color.b(),
                        42,
                    ),
                );
                painter.line_segment(
                    [pos2(loop_left, rect.top()), pos2(loop_left, rect.bottom())],
                    Stroke::new(1.5_f32, loop_color),
                );
                painter.line_segment(
                    [
                        pos2(loop_right, rect.top()),
                        pos2(loop_right, rect.bottom()),
                    ],
                    Stroke::new(1.5_f32, loop_color),
                );
            }
        }

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
                let loop_color = Theme::loop_region();
                Color32::from_rgba_unmultiplied(loop_color.r(), loop_color.g(), loop_color.b(), 225)
            } else {
                Theme::wave_fill()
            };

            painter.line_segment(
                [pos2(x, y_top), pos2(x, y_bot)],
                Stroke::new((bucket_w * 0.82).clamp(0.8, 1.5), color),
            );
        }

        // The selected automation lane is drawn over the waveform. Each lane
        // keeps its own independent point list and can be enabled simultaneously.
        if lane_enabled && automation_points.len() >= 2 {
            let envelope_color = match automation_lane {
                WaveformAutomationLane::Volume => Theme::volume_automation(),
                WaveformAutomationLane::Speed => Theme::accent(),
                WaveformAutomationLane::Pitch => Theme::accent2(),
            };
            for pair in automation_points.windows(2) {
                let x1 = secs_to_x(pair[0][0]);
                let x2 = secs_to_x(pair[1][0]);
                if x2 < rect.left() || x1 > rect.right() {
                    continue;
                }
                painter.line_segment(
                    [
                        pos2(x1, value_to_y(pair[0][1])),
                        pos2(x2, value_to_y(pair[1][1])),
                    ],
                    Stroke::new(2.5_f32, envelope_color),
                );
            }
            for (index, point) in automation_points.iter().enumerate() {
                let x = secs_to_x(point[0]);
                if x < rect.left() - 8.0 || x > rect.right() + 8.0 {
                    continue;
                }
                let center = pos2(x, value_to_y(point[1]));
                painter.circle_filled(center, 5.5, envelope_color);
                painter.circle_stroke(center, 5.5, Stroke::new(1.2_f32, Color32::WHITE));
                if index == 0 || index + 1 == automation_points.len() {
                    painter.circle_filled(center, 2.0, Color32::WHITE);
                }
            }
        }

        // Линии Fade In и Fade Out
        if cue.fade_in_secs > 0.0 {
            let x0 = secs_to_x(cue.start_secs);
            let x1 = secs_to_x((cue.start_secs + cue.fade_in_secs).min(eff_end));
            painter.line_segment(
                [pos2(x0, rect.bottom() - 14.0), pos2(x1, rect.top() + 8.0)],
                Stroke::new(2.0_f32, Theme::accent2()),
            );
        }
        if cue.fade_out_secs > 0.0 {
            let x0 = secs_to_x((eff_end - cue.fade_out_secs).max(cue.start_secs));
            let x1 = secs_to_x(eff_end);
            painter.line_segment(
                [pos2(x0, rect.top() + 8.0), pos2(x1, rect.bottom() - 14.0)],
                Stroke::new(2.0_f32, Theme::accent()),
            );
        }

        // Центральная ось
        painter.line_segment(
            [pos2(rect.left(), mid_y), pos2(rect.right(), mid_y)],
            Stroke::new(0.5_f32, Theme::border()),
        );

        // Playhead / stored seek position. A paused or manually positioned cue
        // uses a yellow marker so the operator can always see the exact point.
        if cue.position_secs.is_finite()
            && (cue.position_secs > 0.0 || cue.is_active() || cue.state == CueState::Paused)
        {
            let px = secs_to_x(cue.position_secs.clamp(0.0, dur));
            let is_paused_or_parked = matches!(&cue.state, CueState::Paused | CueState::Stopped);
            let playhead_color = if is_paused_or_parked {
                Color32::from_rgb(255, 205, 45)
            } else {
                Color32::WHITE
            };
            if is_paused_or_parked {
                painter.line_segment(
                    [pos2(px, rect.top()), pos2(px, rect.bottom())],
                    Stroke::new(6.0_f32, Color32::from_rgba_unmultiplied(255, 205, 45, 55)),
                );
            }
            painter.line_segment(
                [pos2(px, rect.top()), pos2(px, rect.bottom())],
                Stroke::new(2.2_f32, playhead_color),
            );
        }

        // Маркеры Start / End / Loop
        Self::draw_marker(
            &painter,
            secs_to_x(cue.start_secs),
            rect,
            Theme::marker_start(),
            "▼ START",
        );

        if cue.end_secs.is_some() {
            Self::draw_marker(
                &painter,
                secs_to_x(eff_end),
                rect,
                Theme::marker_end(),
                "▼ END",
            );
        }

        if cue.loop_region.enabled {
            Self::draw_marker(
                &painter,
                secs_to_x(cue.loop_region.start_secs),
                rect,
                Theme::marker_loop(),
                "▼ L-IN",
            );
            Self::draw_marker(
                &painter,
                secs_to_x(cue.loop_region.end_secs),
                rect,
                Theme::marker_loop(),
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
                    Stroke::new(1.0_f32, Theme::border()),
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

        // Volume automation editing: drag a node, click the envelope to add a
        // breakpoint, and right-click an interior node to remove it.
        let drag_id = Id::new(("waveform_automation_drag", cue.id, automation_lane));
        let mut dragging_point = ui.ctx().data(|data| data.get_temp::<usize>(drag_id));
        if lane_enabled && resp.drag_started() {
            if let Some(pointer) = resp.interact_pointer_pos() {
                let nearest = automation_points
                    .iter()
                    .enumerate()
                    .map(|(index, point)| {
                        let point_pos = pos2(secs_to_x(point[0]), value_to_y(point[1]));
                        (index, point_pos.distance(pointer))
                    })
                    .min_by(|a, b| a.1.total_cmp(&b.1));
                dragging_point = nearest
                    .filter(|(_, distance)| *distance <= 13.0)
                    .map(|(index, _)| index);
                if let Some(index) = dragging_point {
                    ui.ctx().data_mut(|data| data.insert_temp(drag_id, index));
                }
            }
        }

        if lane_enabled && resp.dragged() {
            if let (Some(index), Some(pointer)) = (dragging_point, resp.interact_pointer_pos()) {
                if index < automation_points.len() {
                    let last_index = automation_points.len().saturating_sub(1);
                    let mut new_time = x_to_secs(pointer.x);
                    if index == 0 {
                        new_time = 0.0;
                    } else if index == last_index {
                        new_time = dur;
                    } else {
                        let min_time = automation_points[index - 1][0] + 0.001;
                        let max_time = automation_points[index + 1][0] - 0.001;
                        new_time = new_time.clamp(min_time.min(max_time), max_time.max(min_time));
                    }
                    let mut new_gain = y_to_value(pointer.y);
                    if automation_lane == WaveformAutomationLane::Speed {
                        new_gain = snap_speed_value(new_gain);
                    }
                    let point = &mut automation_points[index];
                    if (point[0] - new_time).abs() > 0.0001 || (point[1] - new_gain).abs() > 0.0001
                    {
                        *point = [new_time, new_gain];
                        match automation_lane {
                            WaveformAutomationLane::Volume => automation_changed = true,
                            _ => time_pitch_changed = true,
                        }
                    }
                    ui.ctx().request_repaint();
                }
            }
        }

        if lane_enabled && resp.secondary_clicked() {
            if let Some(pointer) = resp.interact_pointer_pos() {
                let nearest = automation_points
                    .iter()
                    .enumerate()
                    .map(|(index, point)| {
                        let point_pos = pos2(secs_to_x(point[0]), value_to_y(point[1]));
                        (index, point_pos.distance(pointer))
                    })
                    .min_by(|a, b| a.1.total_cmp(&b.1));
                if let Some((index, distance)) = nearest {
                    let last_index = automation_points.len().saturating_sub(1);
                    if distance <= 13.0 && index > 0 && index < last_index {
                        automation_points.remove(index);
                        match automation_lane {
                            WaveformAutomationLane::Volume => automation_changed = true,
                            _ => time_pitch_changed = true,
                        }
                    }
                }
            }
        }

        if resp.drag_stopped() {
            ui.ctx().data_mut(|data| data.remove_temp::<usize>(drag_id));
            dragging_point = None;
        }

        // A click near the envelope adds a point; a click elsewhere seeks.
        // This keeps the common playhead interaction while making automation
        // editing feel like a DAW clip envelope.
        let mut seek_position = None;
        if resp.clicked() && dragging_point.is_none() {
            if let Some(pointer) = resp.interact_pointer_pos() {
                let clicked_time = x_to_secs(pointer.x);
                let mut added_point = false;
                if lane_enabled && automation_points.len() >= 2 {
                    let nearest_point = automation_points
                        .iter()
                        .enumerate()
                        .map(|(index, point)| {
                            let point_pos = pos2(secs_to_x(point[0]), value_to_y(point[1]));
                            (index, point_pos.distance(pointer))
                        })
                        .min_by(|a, b| a.1.total_cmp(&b.1));
                    let near_existing = nearest_point
                        .map(|(_, distance)| distance <= 13.0)
                        .unwrap_or(false);
                    if !near_existing {
                        let current_gain = automation_value_at_ui(
                            clicked_time,
                            &automation_points,
                            value_min,
                            value_max,
                        );
                        let curve_y = value_to_y(current_gain);
                        if (pointer.y - curve_y).abs() <= 12.0 {
                            let mut new_value = y_to_value(pointer.y);
                            if automation_lane == WaveformAutomationLane::Speed {
                                new_value = snap_speed_value(new_value);
                            }
                            automation_points.push([clicked_time, new_value]);
                            automation_points.sort_by(|a, b| a[0].total_cmp(&b[0]));
                            match automation_lane {
                                WaveformAutomationLane::Volume => automation_changed = true,
                                _ => time_pitch_changed = true,
                            }
                            added_point = true;
                        }
                    }
                }
                if !added_point {
                    let near_point = lane_enabled
                        && automation_points.iter().any(|point| {
                            pos2(secs_to_x(point[0]), value_to_y(point[1])).distance(pointer)
                                <= 13.0
                        });
                    if !near_point {
                        let position = clicked_time.clamp(cue.start_secs, eff_end);
                        cue.position_secs = position;
                        cue.state = CueState::Paused;
                        seek_position = Some(position);
                    }
                }
            }
        }

        // Dragging near a region marker edits it instead of seeking or drawing
        // automation. A volume node takes precedence when one was grabbed.
        if resp.dragged() && dragging_point.is_none() {
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
            painter.rect_filled(track, 6.0, Theme::bg_panel());
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
            painter.rect_filled(thumb_rect, 7.0, Theme::accent());
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

        Self::store_automation_lane(cue, automation_lane, lane_enabled, automation_points);
        (seek_position, automation_changed, time_pitch_changed)
    }

    fn store_automation_lane(
        cue: &mut Cue,
        lane: WaveformAutomationLane,
        enabled: bool,
        points: Vec<[f64; 2]>,
    ) {
        match lane {
            WaveformAutomationLane::Volume => {
                cue.volume_automation_enabled = enabled;
                cue.volume_automation = points;
            }
            WaveformAutomationLane::Speed => {
                cue.time_pitch.speed_automation_enabled = enabled;
                cue.time_pitch.speed_automation = points;
            }
            WaveformAutomationLane::Pitch => {
                cue.time_pitch.pitch_automation_enabled = enabled;
                cue.time_pitch.pitch_automation = points;
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

fn automation_value_at_ui(
    position_secs: f64,
    points: &[[f64; 2]],
    min_value: f64,
    max_value: f64,
) -> f64 {
    if points.is_empty() {
        return min_value;
    }
    if position_secs <= points[0][0] {
        return points[0][1].clamp(min_value, max_value);
    }
    for pair in points.windows(2) {
        let left = pair[0];
        let right = pair[1];
        if position_secs <= right[0] {
            let span = right[0] - left[0];
            if span <= f64::EPSILON {
                return right[1].clamp(min_value, max_value);
            }
            let t = ((position_secs - left[0]) / span).clamp(0.0, 1.0);
            return (left[1] + (right[1] - left[1]) * t).clamp(min_value, max_value);
        }
    }
    points
        .last()
        .map(|point| point[1].clamp(min_value, max_value))
        .unwrap_or(min_value)
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

#[cfg(test)]
mod speed_automation_tests {
    use super::snap_speed_value;

    #[test]
    fn speed_automation_uses_tenth_steps_and_valid_range() {
        assert!((snap_speed_value(1.23) - 1.2).abs() < f64::EPSILON);
        assert!((snap_speed_value(1.26) - 1.3).abs() < f64::EPSILON);
        assert!((snap_speed_value(0.01) - 0.1).abs() < f64::EPSILON);
        assert!((snap_speed_value(5.0) - 4.0).abs() < f64::EPSILON);
    }
}
