use crate::cue::{Cue, CueType};
use crate::ui::cue_table::format_timecode;
use crate::ui::theme::Theme;
use egui::*;
use uuid::Uuid;

const LABEL_WIDTH: f32 = 250.0;
const RULER_HEIGHT: f32 = 30.0;
const DEFAULT_ROW_HEIGHT: f32 = 48.0;
const MIN_EVENT_DURATION: f64 = 0.05;

fn zoom_row_height(current: f32, scroll_delta_y: f32) -> f32 {
    (current * (scroll_delta_y * 0.0015).exp()).clamp(28.0, 112.0)
}

#[derive(Debug, Clone, Copy)]
enum DragKind {
    Move,
    LeftEdge,
    RightEdge,
}

#[derive(Debug, Clone, Copy)]
struct TimelineDrag {
    cue_id: Uuid,
    kind: DragKind,
    pointer_start_x: f32,
    initial_pre_wait: f64,
    initial_audio_start: f64,
    initial_audio_end: f64,
    initial_duration: f64,
}

#[derive(Debug, Clone, Copy)]
struct TimelineViewState {
    pixels_per_second: f32,
    row_height: f32,
    horizontal_offset: f32,
    snap_grid: bool,
    snap_start: bool,
    snap_end: bool,
    grid_step_secs: f64,
}

impl Default for TimelineViewState {
    fn default() -> Self {
        Self {
            pixels_per_second: 72.0,
            row_height: DEFAULT_ROW_HEIGHT,
            horizontal_offset: 0.0,
            snap_grid: true,
            snap_start: true,
            snap_end: true,
            grid_step_secs: 0.5,
        }
    }
}

pub struct TimelineEditor;

impl TimelineEditor {
    pub fn show(
        ui: &mut Ui,
        group_id: Uuid,
        selected_id: Option<Uuid>,
        cues: &mut [Cue],
        playhead_secs: Option<f64>,
    ) -> Option<Uuid> {
        let group_snapshot = cues.iter().find(|cue| cue.id == group_id).cloned();
        let Some(group_snapshot) = group_snapshot else {
            ui.label("The selected Timeline group no longer exists.");
            return None;
        };
        let CueType::Group {
            mode: crate::cue::GroupMode::Timeline,
            children,
            ..
        } = group_snapshot.cue_type
        else {
            ui.label("Select a group whose mode is Timeline.");
            return None;
        };

        let view_id = Id::new(("timeline_view_state", group_id));
        let drag_id = Id::new(("timeline_drag_state", group_id));
        let mut view = ui
            .ctx()
            .data(|data| data.get_temp::<TimelineViewState>(view_id))
            .unwrap_or_default();
        let mut selected_cue = None;

        let max_time = children
            .iter()
            .filter_map(|id| cues.iter().find(|cue| cue.id == *id))
            .map(|cue| cue.pre_wait_secs.max(0.0) + timeline_duration(cue).max(0.0))
            .fold(10.0_f64, f64::max)
            .max(1.0);

        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Timeline").strong().color(Theme::accent()));
            ui.separator();
            if ui.small_button("−").clicked() {
                view.pixels_per_second = (view.pixels_per_second / 1.2).clamp(12.0, 520.0);
            }
            if ui.small_button("+").clicked() {
                view.pixels_per_second = (view.pixels_per_second * 1.2).clamp(12.0, 520.0);
            }
            ui.label(format!("{:.0} px/s", view.pixels_per_second));
            ui.add_sized(
                [120.0, 18.0],
                Slider::new(&mut view.pixels_per_second, 12.0..=520.0).show_value(false),
            );
            ui.label(format!("Y: {:.0} px", view.row_height));
            ui.add_sized(
                [96.0, 18.0],
                Slider::new(&mut view.row_height, 28.0..=112.0).show_value(false),
            );
            ui.label(
                RichText::new("Alt+Wheel: X · Ctrl+Wheel: Y · Shift+Wheel: pan")
                    .color(Theme::text_dim())
                    .size(Theme::scaled_font_size(9.0)),
            );
            ui.separator();
            ui.checkbox(&mut view.snap_grid, "Snap Grid");
            ui.checkbox(&mut view.snap_start, "Snap Start");
            ui.checkbox(&mut view.snap_end, "Snap End");
            ComboBox::from_id_salt(("timeline_grid_step", group_id))
                .selected_text(format!("{} s", view.grid_step_secs))
                .show_ui(ui, |ui| {
                    for step in [0.1_f64, 0.25, 0.5, 1.0, 2.0, 5.0] {
                        ui.selectable_value(&mut view.grid_step_secs, step, format!("{} s", step));
                    }
                });
        });

        let interaction_rect = ui.available_rect_before_wrap();
        let (hover_pos, shift_held, alt_held, ctrl_held, scroll_delta) = ui.input(|input| {
            let raw_delta = input.raw_scroll_delta;
            let smooth_delta = input.smooth_scroll_delta;
            let scroll_delta = if raw_delta.length_sq() > f32::EPSILON {
                raw_delta
            } else {
                smooth_delta
            };
            (
                input.pointer.hover_pos(),
                input.modifiers.shift,
                input.modifiers.alt,
                input.modifiers.ctrl,
                scroll_delta,
            )
        });
        if let Some(pointer) = hover_pos.filter(|pointer| interaction_rect.contains(*pointer)) {
            if ctrl_held && scroll_delta.y.abs() > f32::EPSILON {
                // Ctrl + wheel scales track height. Prefer raw wheel input because
                // smooth_scroll_delta may be zero on the first native wheel frame.
                view.row_height = zoom_row_height(view.row_height, scroll_delta.y);
                ui.ctx().input_mut(|input| {
                    input.raw_scroll_delta = Vec2::ZERO;
                    input.smooth_scroll_delta = Vec2::ZERO;
                });
                ui.ctx().request_repaint();
            } else if pointer.x >= interaction_rect.left() + LABEL_WIDTH {
                if shift_held {
                    let pan_delta = if scroll_delta.x.abs() > f32::EPSILON {
                        scroll_delta.x
                    } else {
                        scroll_delta.y
                    };
                    if pan_delta.abs() > f32::EPSILON {
                        view.horizontal_offset = (view.horizontal_offset - pan_delta).max(0.0);
                        ui.ctx().input_mut(|input| {
                            input.raw_scroll_delta = Vec2::ZERO;
                            input.smooth_scroll_delta = Vec2::ZERO;
                        });
                    }
                } else if alt_held && scroll_delta.y.abs() > f32::EPSILON {
                    // Alt + wheel zooms the time axis around the pointer.
                    let old_zoom = view.pixels_per_second.max(1.0);
                    let pointer_offset = pointer.x - (interaction_rect.left() + LABEL_WIDTH);
                    let anchor_time = (view.horizontal_offset + pointer_offset) / old_zoom;
                    view.pixels_per_second =
                        (old_zoom * (scroll_delta.y * 0.0015).exp()).clamp(12.0, 520.0);
                    view.horizontal_offset =
                        (anchor_time * view.pixels_per_second - pointer_offset).max(0.0);
                    ui.ctx().input_mut(|input| {
                        input.raw_scroll_delta = Vec2::ZERO;
                        input.smooth_scroll_delta = Vec2::ZERO;
                    });
                }
            }
        }

        let available_width = ui.available_width().max(LABEL_WIDTH + 120.0);
        let timeline_view_width = (available_width - LABEL_WIDTH).max(120.0);
        let timeline_canvas_width = (max_time as f32 * view.pixels_per_second)
            .max(timeline_view_width + view.horizontal_offset);
        let max_horizontal_offset = (timeline_canvas_width - timeline_view_width).max(0.0);
        view.horizontal_offset = view.horizontal_offset.clamp(0.0, max_horizontal_offset);
        if max_horizontal_offset > 0.0 {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Horizontal view")
                        .color(Theme::text_dim())
                        .size(Theme::scaled_font_size(9.5)),
                );
                ui.add_sized(
                    [220.0, 16.0],
                    Slider::new(&mut view.horizontal_offset, 0.0..=max_horizontal_offset)
                        .show_value(false),
                );
            });
        }

        let snap_points: Vec<(Uuid, f64, f64)> = children
            .iter()
            .filter_map(|id| cues.iter().find(|cue| cue.id == *id))
            .map(|cue| {
                (
                    cue.id,
                    cue.pre_wait_secs.max(0.0),
                    cue.pre_wait_secs.max(0.0) + timeline_duration(cue),
                )
            })
            .collect();

        // Keyboard editing is active while the pointer is over the Timeline
        // canvas, so normal Cue List navigation remains untouched elsewhere.
        let pointer_over_timeline =
            ui.input(|input| input.pointer.hover_pos())
                .is_some_and(|pointer| {
                    interaction_rect.contains(pointer)
                        && pointer.x >= interaction_rect.left() + LABEL_WIDTH
                });
        if pointer_over_timeline {
            let (left, right, up, down) = ui.input_mut(|input| {
                (
                    input.consume_key(Modifiers::NONE, Key::ArrowLeft),
                    input.consume_key(Modifiers::NONE, Key::ArrowRight),
                    input.consume_key(Modifiers::NONE, Key::ArrowUp),
                    input.consume_key(Modifiers::NONE, Key::ArrowDown),
                )
            });
            if let Some(selected_id) = selected_id.filter(|id| children.contains(id)) {
                if left || right {
                    let step = if view.snap_grid {
                        view.grid_step_secs.max(0.01)
                    } else {
                        0.1
                    };
                    if let Some(cue) = cues.iter_mut().find(|cue| cue.id == selected_id) {
                        let direction = if left { -1.0 } else { 1.0 };
                        cue.pre_wait_secs = snap_time(
                            cue.pre_wait_secs + direction * step,
                            cue.id,
                            view,
                            &snap_points,
                            shift_held,
                        )
                        .clamp(0.0, 3600.0);
                    }
                }
                if up || down {
                    if let Some(index) = children.iter().position(|id| *id == selected_id) {
                        let next_index = if up {
                            index.saturating_sub(1)
                        } else {
                            (index + 1).min(children.len().saturating_sub(1))
                        };
                        if let Some(next_id) = children.get(next_index) {
                            selected_cue = Some(*next_id);
                        }
                    }
                }
            }
        }

        // Time ruler. The label column remains fixed while only the time canvas
        // moves horizontally, matching the two-pane QLab-style layout.
        ui.horizontal(|ui| {
            let (label_rect, _) =
                ui.allocate_exact_size(vec2(LABEL_WIDTH, RULER_HEIGHT), Sense::hover());
            ui.painter().rect_filled(label_rect, 0.0, Theme::bg_panel());
            ui.painter().text(
                label_rect.left_center() + vec2(8.0, 0.0),
                Align2::LEFT_CENTER,
                "CUE / TYPE / NAME",
                FontId::proportional(Theme::scaled_font_size(10.0)),
                Theme::text_dim(),
            );
            let (ruler_rect, _) =
                ui.allocate_exact_size(vec2(timeline_view_width, RULER_HEIGHT), Sense::hover());
            ui.painter().rect_filled(ruler_rect, 0.0, Theme::bg_panel());
            let painter = ui.painter().with_clip_rect(ruler_rect);
            let tick_step = ruler_tick_step(view.grid_step_secs, view.pixels_per_second);
            let pixels_per_second = view.pixels_per_second.max(1.0) as f64;
            let horizontal_offset = view.horizontal_offset as f64;
            let viewport_width = timeline_view_width as f64;
            let first_tick =
                (horizontal_offset / pixels_per_second / tick_step).floor() * tick_step;
            let last_tick = (horizontal_offset + viewport_width) / pixels_per_second;
            let mut tick = first_tick;
            while tick <= last_tick + tick_step {
                let x = ruler_rect.left() + tick as f32 * view.pixels_per_second
                    - view.horizontal_offset;
                if x >= ruler_rect.left() - 1.0 && x <= ruler_rect.right() + 1.0 {
                    painter.line_segment(
                        [
                            pos2(x, ruler_rect.top() + 17.0),
                            pos2(x, ruler_rect.bottom()),
                        ],
                        Stroke::new(1.0_f32, Theme::border()),
                    );
                    painter.text(
                        pos2(x + 3.0, ruler_rect.top() + 8.0),
                        Align2::LEFT_CENTER,
                        format_timecode(tick),
                        FontId::monospace(Theme::scaled_font_size(9.5)),
                        Theme::text_dim(),
                    );
                }
                tick += tick_step;
            }
        });

        let drag_snapshot = ui
            .ctx()
            .data(|data| data.get_temp::<Option<TimelineDrag>>(drag_id))
            .flatten();
        let mut boundary_guide_xs = Vec::<f32>::new();
        if let Some(active_drag) = drag_snapshot {
            for (other_id, start, end) in &snap_points {
                if *other_id == active_drag.cue_id {
                    continue;
                }
                boundary_guide_xs
                    .push(*start as f32 * view.pixels_per_second - view.horizontal_offset);
                if (*end - *start).abs() > 0.001 {
                    boundary_guide_xs
                        .push(*end as f32 * view.pixels_per_second - view.horizontal_offset);
                }
            }
        }
        let mut drag_finished = false;

        ScrollArea::vertical()
            .id_salt(("timeline_rows", group_id))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (row_index, cue_id) in children.iter().enumerate() {
                    let Some(cue) = cues.iter_mut().find(|cue| cue.id == *cue_id) else {
                        continue;
                    };
                    let cue_color = cue.color.to_egui();
                    let is_selected = selected_id == Some(cue.id)
                        || drag_snapshot.is_some_and(|drag| drag.cue_id == cue.id);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        let (label_rect, _) = ui.allocate_exact_size(
                            vec2(LABEL_WIDTH, view.row_height),
                            Sense::hover(),
                        );
                        ui.painter().rect_filled(
                            label_rect,
                            0.0,
                            if is_selected {
                                Theme::accent().linear_multiply(0.20)
                            } else if row_index % 2 == 0 {
                                Theme::bg_dark()
                            } else {
                                Theme::bg_panel()
                            },
                        );
                        ui.painter().rect_filled(
                            Rect::from_min_max(
                                label_rect.left_top(),
                                pos2(label_rect.left() + 4.0, label_rect.bottom()),
                            ),
                            0.0,
                            cue_color,
                        );
                        let label_response = ui.interact(
                            label_rect,
                            Id::new(("timeline_event_label", group_id, cue.id)),
                            Sense::click(),
                        );
                        if label_response.clicked() {
                            selected_cue = Some(cue.id);
                        }
                        let label = format!(
                            "{}  {}\n{}",
                            if cue.number.trim().is_empty() {
                                "—"
                            } else {
                                cue.number.as_str()
                            },
                            cue.name,
                            cue.cue_type.label()
                        );
                        ui.painter().text(
                            label_rect.left_top() + vec2(12.0, 7.0),
                            Align2::LEFT_TOP,
                            label,
                            FontId::proportional(Theme::scaled_font_size(10.5)),
                            Theme::text(),
                        );

                        let (viewport_rect, _) = ui.allocate_exact_size(
                            vec2(timeline_view_width, view.row_height),
                            Sense::hover(),
                        );
                        let painter = ui.painter().with_clip_rect(viewport_rect);
                        painter.rect_filled(
                            viewport_rect,
                            0.0,
                            if row_index % 2 == 0 {
                                Theme::bg_dark()
                            } else {
                                Theme::bg_panel()
                            },
                        );

                        let tick_step =
                            ruler_tick_step(view.grid_step_secs, view.pixels_per_second);
                        let pixels_per_second = view.pixels_per_second.max(1.0) as f64;
                        let horizontal_offset = view.horizontal_offset as f64;
                        let viewport_width = timeline_view_width as f64;
                        let first_tick =
                            (horizontal_offset / pixels_per_second / tick_step).floor() * tick_step;
                        let last_tick = (horizontal_offset + viewport_width) / pixels_per_second;
                        let mut tick = first_tick;
                        while tick <= last_tick + tick_step {
                            let x = viewport_rect.left() + tick as f32 * view.pixels_per_second
                                - view.horizontal_offset;
                            if x >= viewport_rect.left() - 1.0 && x <= viewport_rect.right() + 1.0 {
                                painter.line_segment(
                                    [
                                        pos2(x, viewport_rect.top()),
                                        pos2(x, viewport_rect.bottom()),
                                    ],
                                    Stroke::new(1.0_f32, Theme::border().linear_multiply(0.55)),
                                );
                            }
                            tick += tick_step;
                        }

                        if let Some(playhead) = playhead_secs {
                            let x = viewport_rect.left() + playhead as f32 * view.pixels_per_second
                                - view.horizontal_offset;
                            if x >= viewport_rect.left() && x <= viewport_rect.right() {
                                painter.line_segment(
                                    [
                                        pos2(x, viewport_rect.top()),
                                        pos2(x, viewport_rect.bottom()),
                                    ],
                                    Stroke::new(1.5_f32, Color32::from_rgb(245, 75, 65)),
                                );
                            }
                        }

                        let start_time = cue.pre_wait_secs.max(0.0);
                        let duration = timeline_duration(cue);
                        let event_left = viewport_rect.left()
                            + start_time as f32 * view.pixels_per_second
                            - view.horizontal_offset;
                        let event_width = if duration > 0.0 {
                            (duration as f32 * view.pixels_per_second).max(18.0)
                        } else {
                            16.0
                        };
                        let event_rect = Rect::from_min_size(
                            pos2(event_left, viewport_rect.top() + 7.0),
                            vec2(event_width, view.row_height - 14.0),
                        );
                        let visible_event_rect = event_rect.intersect(viewport_rect);
                        if visible_event_rect.is_positive() {
                            let fill = if is_selected {
                                Color32::from_rgba_unmultiplied(255, 232, 145, 61)
                            } else {
                                cue_color.linear_multiply(0.24)
                            };
                            painter.rect_filled(event_rect, 4.0, fill);
                            painter.rect_stroke(
                                event_rect,
                                4.0,
                                Stroke::new(
                                    if is_selected { 2.0_f32 } else { 1.2_f32 },
                                    if is_selected {
                                        Theme::accent()
                                    } else {
                                        cue_color
                                    },
                                ),
                                StrokeKind::Inside,
                            );

                            match &cue.cue_type {
                                CueType::Audio | CueType::Play { target_id: None }
                                    if !cue.file_path.as_os_str().is_empty() =>
                                {
                                    draw_waveform(&painter, cue, event_rect, cue_color);
                                    painter.text(
                                        event_rect.left_top() + vec2(5.0, 2.0),
                                        Align2::LEFT_TOP,
                                        cue.name.as_str(),
                                        FontId::proportional(Theme::scaled_font_size(9.0)),
                                        Theme::text(),
                                    );
                                }
                                CueType::Fade { fade_in, .. } => {
                                    painter.text(
                                        event_rect.center(),
                                        Align2::CENTER_CENTER,
                                        if *fade_in { "FADE IN" } else { "FADE OUT" },
                                        FontId::proportional(Theme::scaled_font_size(9.0)),
                                        cue_color,
                                    );
                                }
                                CueType::Midi { .. } => {
                                    painter.text(
                                        event_rect.center(),
                                        Align2::CENTER_CENTER,
                                        "MIDI",
                                        FontId::proportional(Theme::scaled_font_size(9.0)),
                                        cue_color,
                                    );
                                }
                                CueType::Osc { .. } => {
                                    painter.text(
                                        event_rect.center(),
                                        Align2::CENTER_CENTER,
                                        "OSC",
                                        FontId::proportional(Theme::scaled_font_size(9.0)),
                                        cue_color,
                                    );
                                }
                                _ => {
                                    painter.text(
                                        event_rect.center(),
                                        Align2::CENTER_CENTER,
                                        cue.cue_type.label(),
                                        FontId::proportional(Theme::scaled_font_size(9.0)),
                                        cue_color,
                                    );
                                }
                            }

                            let duration_editable =
                                matches!(&cue.cue_type, CueType::Audio | CueType::Fade { .. })
                                    || (matches!(&cue.cue_type, CueType::Play { target_id: None })
                                        && !cue.file_path.as_os_str().is_empty());
                            let body_rect = if duration_editable {
                                Rect::from_min_max(
                                    pos2(event_rect.left() + 6.0, event_rect.top()),
                                    pos2(event_rect.right() - 6.0, event_rect.bottom()),
                                )
                            } else {
                                event_rect
                            };
                            let body_response = ui.interact(
                                body_rect.intersect(viewport_rect),
                                Id::new(("timeline_event_body", group_id, cue.id)),
                                Sense::click_and_drag(),
                            );
                            if body_response.clicked() {
                                selected_cue = Some(cue.id);
                            }
                            if body_response.hovered() {
                                ui.ctx().set_cursor_icon(CursorIcon::Grab);
                            }
                            if body_response.drag_started() {
                                selected_cue = Some(cue.id);
                                if let Some(pointer) =
                                    ui.input(|input| input.pointer.interact_pos())
                                {
                                    ui.ctx().data_mut(|data| {
                                        data.insert_temp(
                                            drag_id,
                                            Some(TimelineDrag {
                                                cue_id: cue.id,
                                                kind: DragKind::Move,
                                                pointer_start_x: pointer.x,
                                                initial_pre_wait: cue.pre_wait_secs,
                                                initial_audio_start: cue.start_secs,
                                                initial_audio_end: cue.effective_end_secs(),
                                                initial_duration: duration,
                                            }),
                                        );
                                    });
                                }
                            }
                            if body_response.dragged() {
                                if let (Some(drag), Some(pointer)) = (
                                    drag_snapshot.filter(|drag| drag.cue_id == cue.id),
                                    ui.input(|input| input.pointer.interact_pos()),
                                ) {
                                    apply_drag(
                                        cue,
                                        drag,
                                        pointer.x,
                                        view.pixels_per_second,
                                        view,
                                        &snap_points,
                                        shift_held,
                                    );
                                }
                            }
                            if body_response.drag_stopped() {
                                drag_finished = true;
                            }

                            if duration_editable {
                                let handle_width = 9.0;
                                let left_handle = Rect::from_min_max(
                                    pos2(event_rect.left() - handle_width * 0.5, event_rect.top()),
                                    pos2(
                                        event_rect.left() + handle_width * 0.5,
                                        event_rect.bottom(),
                                    ),
                                );
                                let right_handle = Rect::from_min_max(
                                    pos2(event_rect.right() - handle_width * 0.5, event_rect.top()),
                                    pos2(
                                        event_rect.right() + handle_width * 0.5,
                                        event_rect.bottom(),
                                    ),
                                );
                                let left_response = ui.interact(
                                    left_handle.intersect(viewport_rect),
                                    Id::new(("timeline_event_left", group_id, cue.id)),
                                    Sense::drag(),
                                );
                                if left_response.drag_started() {
                                    selected_cue = Some(cue.id);
                                    if let Some(pointer) =
                                        ui.input(|input| input.pointer.interact_pos())
                                    {
                                        ui.ctx().data_mut(|data| {
                                            data.insert_temp(
                                                drag_id,
                                                Some(TimelineDrag {
                                                    cue_id: cue.id,
                                                    kind: DragKind::LeftEdge,
                                                    pointer_start_x: pointer.x,
                                                    initial_pre_wait: cue.pre_wait_secs,
                                                    initial_audio_start: cue.start_secs,
                                                    initial_audio_end: cue.effective_end_secs(),
                                                    initial_duration: duration,
                                                }),
                                            );
                                        });
                                    }
                                }
                                if left_response.dragged() {
                                    if let (Some(drag), Some(pointer)) = (
                                        drag_snapshot.filter(|drag| drag.cue_id == cue.id),
                                        ui.input(|input| input.pointer.interact_pos()),
                                    ) {
                                        apply_drag(
                                            cue,
                                            drag,
                                            pointer.x,
                                            view.pixels_per_second,
                                            view,
                                            &snap_points,
                                            shift_held,
                                        );
                                    }
                                }
                                if left_response.drag_stopped() {
                                    drag_finished = true;
                                }

                                let right_response = ui.interact(
                                    right_handle.intersect(viewport_rect),
                                    Id::new(("timeline_event_right", group_id, cue.id)),
                                    Sense::drag(),
                                );
                                if right_response.drag_started() {
                                    selected_cue = Some(cue.id);
                                    if let Some(pointer) =
                                        ui.input(|input| input.pointer.interact_pos())
                                    {
                                        ui.ctx().data_mut(|data| {
                                            data.insert_temp(
                                                drag_id,
                                                Some(TimelineDrag {
                                                    cue_id: cue.id,
                                                    kind: DragKind::RightEdge,
                                                    pointer_start_x: pointer.x,
                                                    initial_pre_wait: cue.pre_wait_secs,
                                                    initial_audio_start: cue.start_secs,
                                                    initial_audio_end: cue.effective_end_secs(),
                                                    initial_duration: duration,
                                                }),
                                            );
                                        });
                                    }
                                }
                                if right_response.dragged() {
                                    if let (Some(drag), Some(pointer)) = (
                                        drag_snapshot.filter(|drag| drag.cue_id == cue.id),
                                        ui.input(|input| input.pointer.interact_pos()),
                                    ) {
                                        apply_drag(
                                            cue,
                                            drag,
                                            pointer.x,
                                            view.pixels_per_second,
                                            view,
                                            &snap_points,
                                            shift_held,
                                        );
                                    }
                                }
                                if right_response.drag_stopped() {
                                    drag_finished = true;
                                }

                                // Edge handles advertise that the event can be resized.
                                if left_response.hovered() || right_response.hovered() {
                                    ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
                                }
                                let handle_color = if is_selected {
                                    Color32::from_rgb(255, 244, 194)
                                } else {
                                    cue_color
                                };
                                painter.rect_filled(
                                    left_handle.shrink2(vec2(2.5, 3.0)),
                                    2.0,
                                    handle_color,
                                );
                                painter.rect_filled(
                                    right_handle.shrink2(vec2(2.5, 3.0)),
                                    2.0,
                                    handle_color,
                                );
                            }
                        }
                        // During editing, draw precomputed Cue boundary guides
                        // over visible rows only.
                        if drag_snapshot.is_some() && viewport_rect.intersects(ui.clip_rect()) {
                            for guide_x in boundary_guide_xs.iter().copied() {
                                let x = viewport_rect.left() + guide_x;
                                if x >= viewport_rect.left() && x <= viewport_rect.right() {
                                    painter.line_segment(
                                        [
                                            pos2(x, viewport_rect.top()),
                                            pos2(x, viewport_rect.bottom()),
                                        ],
                                        Stroke::new(
                                            0.8_f32,
                                            Color32::from_rgba_unmultiplied(245, 72, 66, 170),
                                        ),
                                    );
                                }
                            }
                        }
                    });
                }
            });

        if drag_finished {
            ui.ctx()
                .data_mut(|data| data.insert_temp(drag_id, None::<TimelineDrag>));
        }

        // Keep the playhead visible when a Timeline is running and make the
        // current zoom/scroll state survive tab switches and repaint cycles.
        ui.ctx().data_mut(|data| data.insert_temp(view_id, view));
        if let Some(id) = selected_cue {
            return Some(id);
        }
        None
    }
}

fn timeline_duration(cue: &Cue) -> f64 {
    match &cue.cue_type {
        CueType::Audio => cue.effective_duration().max(0.0),
        CueType::Play { target_id: None } if !cue.file_path.as_os_str().is_empty() => {
            (cue.effective_end_secs() - cue.start_secs).max(0.0)
        }
        CueType::Fade { duration_secs, .. } => duration_secs.max(0.0),
        _ => 0.0,
    }
}

fn ruler_tick_step(grid_step: f64, pixels_per_second: f32) -> f64 {
    let desired = 64.0_f32 / pixels_per_second.max(1.0);
    let candidates = [0.1_f64, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0];
    candidates
        .into_iter()
        .find(|step| *step >= desired as f64 && *step >= grid_step.min(0.1))
        .unwrap_or(60.0)
}

fn draw_waveform(painter: &Painter, cue: &Cue, rect: Rect, _color: Color32) {
    let buckets = cue.waveform.len() / 2;
    if buckets == 0 || cue.duration_secs <= 0.0 {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "Waveform loading…",
            FontId::proportional(Theme::scaled_font_size(9.0)),
            Theme::text_dim(),
        );
        return;
    }

    let source_start = cue.start_secs.max(0.0);
    let source_end = cue.effective_end_secs().min(cue.duration_secs);
    let source_span = (source_end - source_start).max(0.001);
    let columns = rect.width().ceil().clamp(1.0, 700.0) as usize;
    let center_y = rect.center().y + 3.0;
    let amplitude = (rect.height() - 12.0).max(2.0) * 0.43;
    // A neutral, high-luminance trace stays readable over every Cue color and
    // remains distinct from the pale-yellow selected-event overlay.
    let waveform_color = Color32::from_rgb(238, 246, 255);
    let clipped = painter.with_clip_rect(rect);

    for column in 0..columns {
        let ratio = column as f64 / columns.max(1) as f64;
        let source_time = source_start + source_span * ratio;
        let bucket = ((source_time / cue.duration_secs) * buckets as f64)
            .floor()
            .clamp(0.0, buckets.saturating_sub(1) as f64) as usize;
        let minimum = cue.waveform[bucket * 2].clamp(-1.0, 1.0);
        let maximum = cue.waveform[bucket * 2 + 1].clamp(-1.0, 1.0);
        let x = rect.left() + (column as f32 / columns.max(1) as f32) * rect.width();
        clipped.line_segment(
            [
                pos2(x, center_y - maximum * amplitude),
                pos2(x, center_y - minimum * amplitude),
            ],
            Stroke::new(1.25_f32, waveform_color),
        );
    }
}

fn snap_time(
    value: f64,
    cue_id: Uuid,
    view: TimelineViewState,
    snap_points: &[(Uuid, f64, f64)],
    shift_held: bool,
) -> f64 {
    let raw_value = value.max(0.0);
    if shift_held {
        return raw_value;
    }

    let mut grid_snapped = raw_value;
    if view.snap_grid {
        let step = view.grid_step_secs.max(0.01);
        grid_snapped = (raw_value / step).round() * step;
    }

    // Event boundaries are compared with the unsnapped pointer time so they
    // remain reachable even when the grid step is relatively coarse.
    let tolerance = 10.0 / view.pixels_per_second.max(1.0) as f64;
    let mut nearest: Option<(f64, f64)> = None;
    for (other_id, other_start, other_end) in snap_points.iter().copied() {
        if other_id == cue_id {
            continue;
        }
        if view.snap_start {
            let distance = (other_start - raw_value).abs();
            if distance <= tolerance && nearest.map_or(true, |(_, best)| distance < best) {
                nearest = Some((other_start, distance));
            }
        }
        if view.snap_end {
            let distance = (other_end - raw_value).abs();
            if distance <= tolerance && nearest.map_or(true, |(_, best)| distance < best) {
                nearest = Some((other_end, distance));
            }
        }
    }
    nearest.map_or(grid_snapped, |(candidate, _)| candidate.max(0.0))
}

fn apply_drag(
    cue: &mut Cue,
    drag: TimelineDrag,
    pointer_x: f32,
    pixels_per_second: f32,
    view: TimelineViewState,
    snap_points: &[(Uuid, f64, f64)],
    shift_held: bool,
) {
    let delta_secs = (pointer_x - drag.pointer_start_x) as f64 / pixels_per_second.max(1.0) as f64;
    match drag.kind {
        DragKind::Move => {
            let next = snap_time(
                drag.initial_pre_wait + delta_secs,
                cue.id,
                view,
                snap_points,
                shift_held,
            );
            cue.pre_wait_secs = next.clamp(0.0, 3600.0);
        }
        DragKind::LeftEdge => match &mut cue.cue_type {
            CueType::Audio | CueType::Play { target_id: None } => {
                let max_start = (drag.initial_audio_end - MIN_EVENT_DURATION).max(0.0);
                let next_start = (drag.initial_audio_start + delta_secs).clamp(0.0, max_start);
                let actual_delta = next_start - drag.initial_audio_start;
                let next_pre_wait = snap_time(
                    drag.initial_pre_wait + actual_delta,
                    cue.id,
                    view,
                    snap_points,
                    shift_held,
                )
                .clamp(0.0, 3600.0);
                let snapped_delta = next_pre_wait - drag.initial_pre_wait;
                cue.start_secs = (drag.initial_audio_start + snapped_delta).clamp(0.0, max_start);
                cue.pre_wait_secs = next_pre_wait.clamp(0.0, 3600.0);
            }
            CueType::Fade { duration_secs, .. } => {
                let next_pre_wait = snap_time(
                    drag.initial_pre_wait + delta_secs,
                    cue.id,
                    view,
                    snap_points,
                    shift_held,
                )
                .clamp(0.0, 3600.0);
                let actual_delta = next_pre_wait - drag.initial_pre_wait;
                cue.pre_wait_secs = next_pre_wait;
                *duration_secs = (drag.initial_duration - actual_delta).max(MIN_EVENT_DURATION);
                cue.duration_secs = *duration_secs;
            }
            _ => {}
        },
        DragKind::RightEdge => {
            let new_end_time = snap_time(
                drag.initial_pre_wait + drag.initial_duration + delta_secs,
                cue.id,
                view,
                snap_points,
                shift_held,
            );
            let next_duration = (new_end_time - drag.initial_pre_wait).max(MIN_EVENT_DURATION);
            match &mut cue.cue_type {
                CueType::Audio | CueType::Play { target_id: None } => {
                    let max_end = cue
                        .duration_secs
                        .max(drag.initial_audio_start + MIN_EVENT_DURATION);
                    let end = (drag.initial_audio_start + next_duration)
                        .clamp(drag.initial_audio_start + MIN_EVENT_DURATION, max_end);
                    cue.end_secs = Some(end);
                }
                CueType::Fade { duration_secs, .. } => {
                    *duration_secs = next_duration;
                    cue.duration_secs = next_duration;
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod timeline_snap_tests {
    use super::{snap_time, zoom_row_height, TimelineViewState};
    use uuid::Uuid;

    #[test]
    fn shift_disables_grid_and_event_boundary_snapping() {
        let cue_id = Uuid::new_v4();
        let other_id = Uuid::new_v4();
        let view = TimelineViewState {
            snap_grid: true,
            snap_start: true,
            snap_end: true,
            grid_step_secs: 0.5,
            ..TimelineViewState::default()
        };
        let points = vec![(other_id, 2.1, 4.1)];

        assert!((snap_time(2.08, cue_id, view, &points, true) - 2.08).abs() < 0.0001);
    }

    #[test]
    fn ctrl_wheel_vertical_zoom_changes_track_height_in_both_directions() {
        assert!(zoom_row_height(48.0, 120.0) > 48.0);
        assert!(zoom_row_height(48.0, -120.0) < 48.0);
    }

    #[test]
    fn vertical_zoom_is_clamped_to_readable_track_heights() {
        assert_eq!(zoom_row_height(48.0, 10_000.0), 112.0);
        assert_eq!(zoom_row_height(48.0, -10_000.0), 28.0);
    }

    #[test]
    fn event_boundary_takes_priority_over_a_coarser_grid() {
        let cue_id = Uuid::new_v4();
        let other_id = Uuid::new_v4();
        let view = TimelineViewState {
            snap_grid: true,
            snap_start: true,
            snap_end: true,
            grid_step_secs: 0.5,
            pixels_per_second: 72.0,
            ..TimelineViewState::default()
        };
        let points = vec![(other_id, 2.1, 4.1)];

        assert!((snap_time(2.08, cue_id, view, &points, false) - 2.1).abs() < 0.0001);
    }
}
