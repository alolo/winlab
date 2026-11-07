// src/ui/cue_table.rs
#![allow(float_literal_f32_fallback)]
use crate::cue::{ContinueMode, Cue, CueColor, CueState, CueType, WaitPhase};
use crate::ui::theme::Theme;
use egui::*;
use std::collections::{HashMap, HashSet};
use std::time::Instant;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditField {
    Number,
    Name,
    PreWait,
    Duration,
    PostWait,
}

/// Returns the cue IDs in the same order in which rows are currently shown.
/// Navigation therefore follows the visible project tree, never cue numbering or
/// the backing flat vector order.
pub fn visible_cue_ids(cues: &[Cue], expanded_groups: &HashSet<Uuid>) -> Vec<Uuid> {
    build_display_order(cues, expanded_groups)
        .into_iter()
        .map(|(index, _)| cues[index].id)
        .collect()
}

/// Visible cue IDs that keyboard navigation may select. Comment rows remain in
/// the table for mouse selection, but are intentionally skipped by navigation.
pub fn visible_navigation_cue_ids(cues: &[Cue], expanded_groups: &HashSet<Uuid>) -> Vec<Uuid> {
    let comment_ids: HashSet<Uuid> = cues
        .iter()
        .filter(|cue| matches!(&cue.cue_type, CueType::Comment { .. }))
        .map(|cue| cue.id)
        .collect();
    visible_cue_ids(cues, expanded_groups)
        .into_iter()
        .filter(|id| !comment_ids.contains(id))
        .collect()
}

fn build_display_order(cues: &[Cue], expanded_groups: &HashSet<Uuid>) -> Vec<(usize, u8)> {
    let child_ids: HashSet<Uuid> = cues
        .iter()
        .filter_map(|cue| match &cue.cue_type {
            CueType::Group { children, .. } => Some(children.iter().copied()),
            _ => None,
        })
        .flatten()
        .collect();

    let mut order = Vec::new();
    let mut visible_visited = HashSet::new();
    let mut structurally_reachable = HashSet::new();

    // First walk all real roots. Structural reachability is tracked separately
    // from visibility: children of a collapsed group are intentionally hidden,
    // not orphaned, and must never be appended by the fallback pass below.
    for (index, cue) in cues.iter().enumerate() {
        if !child_ids.contains(&cue.id) {
            collect_structural_reachability(cues, cue.id, &mut structurally_reachable);
            if visible_visited.insert(cue.id) {
                order.push((index, 0));
                append_group_children(
                    cues,
                    cue.id,
                    1,
                    expanded_groups,
                    &mut visible_visited,
                    &mut order,
                );
            }
        }
    }

    // Detached malformed components (for example a cycle with no root) remain
    // reachable, but each component gets only one visible fallback root.
    for (index, cue) in cues.iter().enumerate() {
        if !structurally_reachable.contains(&cue.id) {
            collect_structural_reachability(cues, cue.id, &mut structurally_reachable);
            if visible_visited.insert(cue.id) {
                order.push((index, 0));
                append_group_children(
                    cues,
                    cue.id,
                    1,
                    expanded_groups,
                    &mut visible_visited,
                    &mut order,
                );
            }
        }
    }
    order
}

fn build_sibling_locations(cues: &[Cue]) -> HashMap<Uuid, (Option<Uuid>, usize)> {
    let mut locations = HashMap::new();
    let mut child_ids = HashSet::new();

    for cue in cues {
        if let CueType::Group { children, .. } = &cue.cue_type {
            for (index, child_id) in children.iter().copied().enumerate() {
                child_ids.insert(child_id);
                locations.entry(child_id).or_insert((Some(cue.id), index));
            }
        }
    }

    let mut root_index = 0usize;
    for cue in cues {
        if !child_ids.contains(&cue.id) {
            locations.entry(cue.id).or_insert((None, root_index));
            root_index += 1;
        }
    }
    locations
}

fn collect_structural_reachability(cues: &[Cue], cue_id: Uuid, reachable: &mut HashSet<Uuid>) {
    if !reachable.insert(cue_id) {
        return;
    }
    if let Some(cue) = cues.iter().find(|cue| cue.id == cue_id) {
        if let CueType::Group { children, .. } = &cue.cue_type {
            for child_id in children {
                collect_structural_reachability(cues, *child_id, reachable);
            }
        }
    }
}

fn append_group_children(
    cues: &[Cue],
    group_id: Uuid,
    depth: u8,
    expanded_groups: &HashSet<Uuid>,
    visited: &mut HashSet<Uuid>,
    order: &mut Vec<(usize, u8)>,
) {
    if !expanded_groups.contains(&group_id) {
        return;
    }
    let Some(group) = cues.iter().find(|cue| cue.id == group_id) else {
        return;
    };
    let CueType::Group { children, .. } = &group.cue_type else {
        return;
    };
    for child_id in children {
        if !visited.insert(*child_id) {
            continue;
        }
        if let Some(index) = cues.iter().position(|cue| cue.id == *child_id) {
            order.push((index, depth));
            append_group_children(
                cues,
                *child_id,
                depth.saturating_add(1),
                expanded_groups,
                visited,
                order,
            );
        }
    }
}

pub struct CueTable;

#[derive(Debug, Clone)]
struct CueDragState {
    source_id: Uuid,
    start_pos: Pos2,
    dragged_ids: Vec<Uuid>,
    active: bool,
}

impl CueTable {
    pub fn show(
        ui: &mut Ui,
        cues: &mut Vec<Cue>,
        selected: &mut Option<Uuid>,
        selected_ids: &mut HashSet<Uuid>,
        selection_anchor: &mut Option<Uuid>,
        editing: &mut Option<(Uuid, EditField, String)>,
        editing_focus_pending: &mut bool,
        column_widths: &mut [f32; 9],
        expanded_groups: &mut HashSet<Uuid>,
    ) -> Option<CueTableAction> {
        let mut action = None;

        let cue_names: std::collections::HashMap<Uuid, String> = cues
            .iter()
            .map(|c| {
                let label = if c.number.trim().is_empty() {
                    c.name.clone()
                } else {
                    format!("#{} {}", c.number, c.name)
                };
                (c.id, label)
            })
            .collect();
        let targetable_ids: HashSet<Uuid> = cues
            .iter()
            .filter(|cue| {
                matches!(&cue.cue_type, CueType::Audio | CueType::Group { .. })
                    || !cue.file_path.as_os_str().is_empty()
            })
            .map(|cue| cue.id)
            .collect();

        let row_h = 28.0_f32;
        // The 26 px gutter is shared by header and rows: 18 px for the group
        // disclosure slot, 5 px for the color strip, and 3 px breathing room.
        // The old Status column (saved width index 7) is merged into Duration;
        // keeping the 9-value settings array preserves compatibility with older
        // user settings files.
        let display_order = build_display_order(cues, expanded_groups);
        let viewport_rect = ui.available_rect_before_wrap();
        let vertical_scrollbar_width =
            if display_order.len() as f32 * row_h + 28.0 > viewport_rect.height() {
                14.0
            } else {
                0.0
            };
        let mut display_widths = [
            column_widths[0],
            column_widths[1],
            column_widths[2],
            column_widths[3],
            column_widths[4],
            column_widths[5] + column_widths[7],
            column_widths[6],
            column_widths[8],
        ];
        let available_columns_width =
            (viewport_rect.width() - 26.0 - vertical_scrollbar_width).max(160.0);
        let base_width = display_widths.iter().sum::<f32>();
        if base_width < available_columns_width {
            let extra = available_columns_width - base_width;
            // Give spare room to the descriptive columns so rows use the full
            // remaining Cue List width instead of leaving a blank strip.
            display_widths[2] += extra * 0.62;
            display_widths[3] += extra * 0.38;
        } else if base_width > available_columns_width {
            let mut deficit = base_width - available_columns_width;
            let minimums = [36.0, 44.0, 100.0, 90.0, 52.0, 86.0, 52.0, 90.0];
            for index in [2usize, 3, 5, 7, 1, 0, 4, 6] {
                let reducible = (display_widths[index] - minimums[index]).max(0.0);
                let reduction = reducible.min(deficit);
                display_widths[index] -= reduction;
                deficit -= reduction;
                if deficit <= 0.1 {
                    break;
                }
            }
            if deficit > 0.1 {
                let current = display_widths.iter().sum::<f32>().max(1.0);
                let scale = (available_columns_width / current).min(1.0);
                for width in &mut display_widths {
                    *width *= scale;
                }
            }
        }
        let table_width = 26.0 + display_widths.iter().sum::<f32>();
        let sibling_locations = build_sibling_locations(cues);
        let group_child_counts: HashMap<Uuid, usize> = cues
            .iter()
            .filter_map(|cue| {
                if let CueType::Group { children, .. } = &cue.cue_type {
                    Some((cue.id, children.len()))
                } else {
                    None
                }
            })
            .collect();
        let display_ids: Vec<Uuid> = display_order
            .iter()
            .map(|(index, _)| cues[*index].id)
            .collect();

        let header_rect = Rect::from_min_size(viewport_rect.min, vec2(table_width, 28.0));
        let mut row_rects: Vec<Rect> = Vec::with_capacity(display_order.len());
        let mut drop_destination: Option<(Option<Uuid>, usize)> = None;
        let mut drop_line_y: Option<f32> = None;
        let drag_state_id = Id::new("cue_table_drag_state");
        let mut drag_state = ui
            .data(|data| data.get_temp::<Option<CueDragState>>(drag_state_id))
            .flatten();

        // The columns are fitted to the available Cue List width, so a horizontal
        // scroll area is no longer needed. The vertical list alone scrolls.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.add_space(26.0);
            let labels = [
                "Cue",
                "Type",
                "Name",
                "File / Target",
                "Pre-Wait",
                "Duration",
                "Post-Wait",
                "Continue",
            ];
            let saved_indices = [0usize, 1, 2, 3, 4, 5, 6, 8];
            for (index, label) in labels.iter().enumerate() {
                const RESIZE_HANDLE_WIDTH: f32 = 14.0;
                ui.add_sized(
                    [
                        (display_widths[index] - RESIZE_HANDLE_WIDTH).max(20.0),
                        22.0,
                    ],
                    Label::new(RichText::new(*label).color(Theme::text_dim()).size(11.0)),
                );
                let (handle_rect, resize_response) =
                    ui.allocate_exact_size(vec2(RESIZE_HANDLE_WIDTH, 22.0), Sense::drag());
                let start_id = Id::new(("cue_column_resize_start", index));
                if resize_response.drag_started() {
                    let start_x = ui
                        .ctx()
                        .pointer_latest_pos()
                        .map(|position| position.x)
                        .unwrap_or(handle_rect.center().x);
                    let saved_width = if index == 5 {
                        column_widths[5] + column_widths[7]
                    } else {
                        column_widths[saved_indices[index]]
                    };
                    ui.data_mut(|data| {
                        data.insert_temp(start_id, (saved_width, start_x));
                    });
                }
                if resize_response.dragged() {
                    if let (Some((start_width, start_x)), Some(pointer)) = (
                        ui.data(|data| data.get_temp::<(f32, f32)>(start_id)),
                        ui.ctx().pointer_latest_pos(),
                    ) {
                        let new_width = (start_width + pointer.x - start_x).clamp(44.0, 600.0);
                        if index == 5 {
                            // Consolidate the legacy Duration + Status
                            // widths into the single visible Duration column.
                            column_widths[5] = new_width;
                            column_widths[7] = 0.0;
                        } else {
                            column_widths[saved_indices[index]] = new_width;
                        }
                    }
                    ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
                } else if resize_response.hovered() {
                    ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
                }
                if resize_response.drag_stopped() {
                    ui.data_mut(|data| {
                        let _ = data.remove_temp::<(f32, f32)>(start_id);
                    });
                }
                ui.painter().line_segment(
                    [
                        pos2(handle_rect.center().x, handle_rect.top() + 3.0),
                        pos2(handle_rect.center().x, handle_rect.bottom() - 3.0),
                    ],
                    Stroke::new(
                        1.5,
                        if resize_response.hovered() || resize_response.dragged() {
                            Theme::accent()
                        } else {
                            Theme::border()
                        },
                    ),
                );
            }
        });
        ui.add(Separator::default().horizontal());

        egui::ScrollArea::vertical()
            .id_salt("cue_table_vertical_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_min_width(table_width);
                let mut completed_group_frames: Vec<Rect> = Vec::new();
                let mut group_frame_stack: Vec<(u8, Rect)> = Vec::new();
                for &(cue_idx, indent) in &display_order {
                    let cue = &mut cues[cue_idx];
                    let is_selected = selected_ids.contains(&cue.id) || *selected == Some(cue.id);
                    let missing_file = matches!(&cue.cue_type, CueType::Audio)
                        && (cue.file_path.as_os_str().is_empty() || !cue.file_path.is_file());
                    let row_start = ui.cursor().min;
                    let row_rect = Rect::from_min_size(row_start, vec2(table_width, row_h));
                    row_rects.push(row_rect);

                    // Close frames for groups whose visible subtree has ended.
                    while group_frame_stack
                        .last()
                        .map(|(depth, _)| *depth >= indent)
                        .unwrap_or(false)
                    {
                        if let Some((_, rect)) = group_frame_stack.pop() {
                            completed_group_frames.push(rect);
                        }
                    }
                    for (_, frame_rect) in group_frame_stack.iter_mut() {
                        *frame_rect = frame_rect.union(row_rect);
                    }
                    let is_group_row = matches!(&cue.cue_type, CueType::Group { .. });
                    // Every group gets its own green frame. A collapsed group
                    // naturally closes after its header row; an expanded group
                    // encloses all visible descendants.
                    if is_group_row {
                        group_frame_stack.push((indent, row_rect));
                    }

                    // Base row background, followed by user cue tint and a subtle state overlay.
                    let base_bg = Self::row_bg(cue, cue_idx);
                    ui.painter().rect_filled(row_rect, 0.0_f32, base_bg);

                    if let Some(cue_tint) = cue.color.row_bg_egui() {
                        ui.painter().rect_filled(row_rect, 0.0_f32, cue_tint);
                    }

                    let is_playing = matches!(
                        &cue.state,
                        CueState::Playing | CueState::FadingIn | CueState::FadingOut
                    );
                    if is_playing {
                        // Playing cue: neutral white overlay at 10% opacity.
                        ui.painter()
                            .rect_filled(row_rect, 0.0_f32, Theme::playing_overlay());
                        ui.painter().rect_stroke(
                            row_rect.shrink(0.5),
                            0.0_f32,
                            Stroke::new(1.0_f32, Theme::playing_outline()),
                            StrokeKind::Inside,
                        );
                    }
                    if is_selected {
                        // Selected cue: accent overlay at 10% opacity (90% transparent).
                        ui.painter()
                            .rect_filled(row_rect, 0.0_f32, Theme::selected_overlay());
                    }

                    // Start a row drag from any cell, including labels, target,
                    // pre/post-wait, status, and the group disclosure area. This
                    // does not depend on which child widget owns the pointer.
                    let pointer_pos = ui.ctx().pointer_latest_pos();
                    let primary_pressed =
                        ui.input(|input| input.pointer.button_pressed(PointerButton::Primary));
                    if drag_state.is_none()
                        && primary_pressed
                        && pointer_pos
                            .map(|position| row_rect.contains(position))
                            .unwrap_or(false)
                    {
                        drag_state = pointer_pos.map(|start_pos| CueDragState {
                            source_id: cue.id,
                            start_pos,
                            dragged_ids: Vec::new(),
                            active: false,
                        });
                    }
                    if let Some(state) = drag_state.as_mut() {
                        if !state.active
                            && ui.input(|input| input.pointer.button_down(PointerButton::Primary))
                        {
                            if let Some(position) = pointer_pos {
                                if state.start_pos.distance(position) >= 6.0 {
                                    state.dragged_ids = if selected_ids.contains(&state.source_id)
                                        && selected_ids.len() > 1
                                    {
                                        display_ids
                                            .iter()
                                            .filter(|id| selected_ids.contains(*id))
                                            .copied()
                                            .collect()
                                    } else {
                                        *selected = Some(state.source_id);
                                        selected_ids.clear();
                                        selected_ids.insert(state.source_id);
                                        *selection_anchor = Some(state.source_id);
                                        vec![state.source_id]
                                    };
                                    state.active = true;
                                }
                            }
                        }
                    }

                    let active_drag_ids: Vec<Uuid> = drag_state
                        .as_ref()
                        .filter(|state| state.active)
                        .map(|state| state.dragged_ids.clone())
                        .unwrap_or_default();
                    if active_drag_ids.contains(&cue.id) {
                        ui.painter().rect_filled(
                            row_rect,
                            2.0_f32,
                            Color32::from_rgba_unmultiplied(90, 170, 255, 55),
                        );
                        ui.painter().rect_stroke(
                            row_rect,
                            2.0_f32,
                            Stroke::new(1.5_f32, Theme::accent()),
                            StrokeKind::Inside,
                        );
                    }

                    if is_selected {
                        ui.painter().rect_stroke(
                            row_rect,
                            2.0_f32,
                            Stroke::new(1.5_f32, Theme::accent()),
                            StrokeKind::Inside,
                        );
                    }

                    if missing_file {
                        // Thin red outline makes unavailable media visible without
                        // changing the cue's normal background or selection state.
                        ui.painter().rect_stroke(
                            row_rect.shrink(0.5),
                            0.0_f32,
                            Stroke::new(1.0_f32, Color32::from_rgb(220, 65, 65)),
                            StrokeKind::Inside,
                        );
                    }

                    // Background row interaction spans every visible column. It is
                    // registered before controls so static cells remain draggable.
                    let row_resp = ui.interact(
                        row_rect,
                        Id::new(("cue_row_click", cue.id)),
                        Sense::click_and_drag(),
                    );
                    let mut target_cell_rect = Rect::NOTHING;
                    let mut cue_number_rect = Rect::NOTHING;
                    let mut name_cell_rect = Rect::NOTHING;
                    let mut pre_wait_rect = Rect::NOTHING;
                    let mut duration_cell_rect = Rect::NOTHING;
                    let mut post_wait_rect = Rect::NOTHING;
                    let mut continue_cell_rect = Rect::NOTHING;
                    let _resp = ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        // Reserve a real horizontal disclosure slot for every row.
                        // ui.add_space() is vertical in egui, so it cannot align columns.
                        let is_group = matches!(&cue.cue_type, CueType::Group { .. });
                        let (arrow_rect, arrow_response) = ui.allocate_exact_size(
                            vec2(18.0, row_h),
                            if is_group {
                                Sense::click()
                            } else {
                                Sense::hover()
                            },
                        );
                        if is_group {
                            let open = expanded_groups.contains(&cue.id);
                            let center = arrow_rect.center();
                            let points = if open {
                                vec![
                                    pos2(center.x - 5.0, center.y - 2.5),
                                    pos2(center.x + 5.0, center.y - 2.5),
                                    pos2(center.x, center.y + 4.5),
                                ]
                            } else {
                                vec![
                                    pos2(center.x - 2.5, center.y - 5.0),
                                    pos2(center.x - 2.5, center.y + 5.0),
                                    pos2(center.x + 4.5, center.y),
                                ]
                            };
                            ui.painter().add(Shape::convex_polygon(
                                points,
                                Theme::text_dim(),
                                Stroke::NONE,
                            ));
                            if arrow_response.clicked() {
                                if open {
                                    expanded_groups.remove(&cue.id);
                                } else {
                                    expanded_groups.insert(cue.id);
                                }
                            }
                        }

                        // Цветная полоска слева
                        let (strip_rect, _) =
                            ui.allocate_exact_size(vec2(5.0, row_h), Sense::hover());
                        ui.painter()
                            .rect_filled(strip_rect, 0.0_f32, cue.color.to_egui());
                        ui.allocate_exact_size(vec2(3.0, row_h), Sense::hover());

                        // Cue number — double-click to edit; an empty value is allowed.
                        let is_editing_number =
                            matches!(editing, Some((id, EditField::Number, _)) if *id == cue.id);
                        if is_editing_number {
                            if let Some((_, _, ref mut buf)) = editing {
                                let response = ui.add_sized(
                                    [display_widths[0], row_h - 4.0],
                                    TextEdit::singleline(buf).font(TextStyle::Monospace),
                                );
                                cue_number_rect = response.rect;
                                if *editing_focus_pending {
                                    response.request_focus();
                                    *editing_focus_pending = false;
                                }
                                if response.lost_focus() || ui.input(|i| i.key_pressed(Key::Enter))
                                {
                                    action = Some(CueTableAction::EditConfirm(
                                        cue.id,
                                        EditField::Number,
                                        buf.clone(),
                                    ));
                                }
                                if ui.input(|i| i.key_pressed(Key::Escape)) {
                                    action = Some(CueTableAction::EditCancel);
                                }
                            }
                        } else {
                            let number_label = ui.add_sized(
                                [display_widths[0], row_h],
                                Label::new(
                                    RichText::new(&cue.number)
                                        .color(Theme::accent())
                                        .monospace()
                                        .size(12.0),
                                )
                                .selectable(false)
                                .sense(Sense::hover()),
                            );
                            cue_number_rect = number_label.rect;
                        }

                        // Type
                        ui.add_sized(
                            [display_widths[1], row_h],
                            Label::new(
                                RichText::new(cue.cue_type.icon())
                                    .color(Theme::text())
                                    .size(15.0),
                            )
                            .selectable(false)
                            .sense(Sense::hover()),
                        )
                        .on_hover_text(cue.cue_type.label());

                        // Name — двойной клик по имени запускает редактирование
                        let is_editing_name =
                            matches!(editing, Some((id, EditField::Name, _)) if *id == cue.id);
                        if is_editing_name {
                            if let Some((_, _, ref mut buf)) = editing {
                                ui.add_space(indent as f32 * 16.0);
                                let r = ui.add_sized(
                                    [
                                        (display_widths[2] - indent as f32 * 16.0).max(24.0),
                                        row_h - 4.0,
                                    ],
                                    TextEdit::singleline(buf).font(TextStyle::Body),
                                );
                                name_cell_rect = r.rect;
                                if *editing_focus_pending {
                                    r.request_focus();
                                    *editing_focus_pending = false;
                                }
                                if r.lost_focus() || ui.input(|i| i.key_pressed(Key::Enter)) {
                                    action = Some(CueTableAction::EditConfirm(
                                        cue.id,
                                        EditField::Name,
                                        buf.clone(),
                                    ));
                                }
                                if ui.input(|i| i.key_pressed(Key::Escape)) {
                                    action = Some(CueTableAction::EditCancel);
                                }
                            }
                        } else {
                            ui.add_space(indent as f32 * 16.0);
                            let name_label = ui.add_sized(
                                [(display_widths[2] - indent as f32 * 16.0).max(24.0), row_h],
                                Label::new(
                                    RichText::new(&cue.name)
                                        .color(Self::name_color(cue))
                                        .size(13.0)
                                        .strong(),
                                )
                                .truncate()
                                .selectable(false)
                                .sense(Sense::hover()),
                            );
                            name_cell_rect = name_label.rect;
                        }

                        // File / Target
                        let target_desc = Self::describe_target(cue, &cue_names);
                        let target_tooltip = target_desc.clone();
                        let target_cell = ui
                            .add_sized(
                                [display_widths[3], row_h],
                                Label::new(
                                    RichText::new(target_desc)
                                        .color(Theme::text_dim())
                                        .size(11.0),
                                )
                                .truncate()
                                .selectable(false)
                                .sense(Sense::hover()),
                            )
                            .on_hover_text(target_tooltip);
                        target_cell_rect = target_cell.rect;

                        // Pre-Wait (двойной клик = редактирование)
                        pre_wait_rect = Self::wait_cell(
                            ui,
                            cue.id,
                            EditField::PreWait,
                            cue.pre_wait_secs,
                            display_widths[4],
                            row_h,
                            Theme::accent(),
                            WaitPhase::Pre,
                            cue.wait_phase,
                            cue.wait_started_at,
                            cue.wait_duration_secs,
                            editing,
                            editing_focus_pending,
                            &mut action,
                            indent,
                        );

                        // Duration. Fade events can be edited directly in the Cue List.
                        let is_fade_cue = matches!(&cue.cue_type, CueType::Fade { .. });
                        let is_editing_duration = matches!(
                            editing,
                            Some((id, EditField::Duration, _)) if *id == cue.id
                        );
                        if is_fade_cue && is_editing_duration {
                            if let Some((_, _, ref mut buf)) = editing {
                                let response = ui.add_sized(
                                    [display_widths[5], row_h - 4.0],
                                    TextEdit::singleline(buf).font(TextStyle::Monospace),
                                );
                                duration_cell_rect = response.rect;
                                if *editing_focus_pending {
                                    response.request_focus();
                                    *editing_focus_pending = false;
                                }
                                if response.lost_focus() || ui.input(|i| i.key_pressed(Key::Enter))
                                {
                                    action = Some(CueTableAction::EditConfirm(
                                        cue.id,
                                        EditField::Duration,
                                        buf.clone(),
                                    ));
                                }
                                if ui.input(|i| i.key_pressed(Key::Escape)) {
                                    action = Some(CueTableAction::EditCancel);
                                }
                            }
                        } else {
                            let (rect, response) = ui.allocate_exact_size(
                                vec2(display_widths[5], row_h - 8.0),
                                Sense::hover(),
                            );
                            duration_cell_rect = rect;
                            let is_playing = matches!(
                                &cue.state,
                                CueState::Playing | CueState::FadingIn | CueState::FadingOut
                            );
                            if is_playing && cue.effective_duration() > 0.0 {
                                let duration = cue.effective_duration();
                                let (progress, remaining) =
                                    if matches!(&cue.cue_type, CueType::Fade { .. }) {
                                        (
                                            (cue.position_secs / duration.max(0.001))
                                                .clamp(0.0, 1.0)
                                                as f32,
                                            (duration - cue.position_secs).max(0.0),
                                        )
                                    } else {
                                        (cue.progress(), cue.remaining_secs())
                                    };
                                let bar_rect = Rect::from_min_size(
                                    rect.min,
                                    vec2(rect.width() * progress, rect.height()),
                                );
                                ui.painter().rect_filled(
                                    bar_rect,
                                    2.0_f32,
                                    Theme::bg_active().linear_multiply(0.8_f32),
                                );
                                ui.painter().text(
                                    rect.center(),
                                    Align2::CENTER_CENTER,
                                    format_duration(remaining),
                                    FontId::monospace(10.0),
                                    Theme::text_active(),
                                );
                            } else if matches!(
                                &cue.state,
                                CueState::Paused | CueState::Loading | CueState::Error(_)
                            ) {
                                let (label, color) = status_label(&cue.state);
                                ui.painter().text(
                                    rect.center(),
                                    Align2::CENTER_CENTER,
                                    label,
                                    FontId::proportional(11.0),
                                    color,
                                );
                            } else if matches!(&cue.cue_type, CueType::Comment { .. }) {
                                ui.painter().text(
                                    rect.center(),
                                    Align2::CENTER_CENTER,
                                    "comment",
                                    FontId::proportional(10.0),
                                    Theme::text_dim(),
                                );
                            } else {
                                let duration = cue.effective_duration();
                                let (label, color) = if duration > 0.0 {
                                    (format_duration(duration), Theme::text_dim())
                                } else if cue.state == CueState::Waiting
                                    && cue.wait_phase != WaitPhase::None
                                {
                                    ("—".to_string(), Theme::text_dim())
                                } else {
                                    let (state, state_color) = status_label(&cue.state);
                                    (state.to_string(), state_color)
                                };
                                ui.painter().text(
                                    rect.center(),
                                    Align2::CENTER_CENTER,
                                    label,
                                    FontId::monospace(10.0),
                                    color,
                                );
                            }
                            let _ = response;
                        }

                        // Post-Wait
                        post_wait_rect = Self::wait_cell(
                            ui,
                            cue.id,
                            EditField::PostWait,
                            cue.post_wait_secs,
                            display_widths[6],
                            row_h,
                            Theme::accent2(),
                            WaitPhase::Post,
                            cue.wait_phase,
                            cue.wait_started_at,
                            cue.wait_duration_secs,
                            editing,
                            editing_focus_pending,
                            &mut action,
                            indent,
                        );

                        // Continue
                        let continue_response = ui.push_id(("continue_combo", cue.id), |ui| {
                            if matches!(&cue.cue_type, CueType::Comment { .. }) {
                                ui.add_sized(
                                    [display_widths[7], row_h],
                                    Label::new(RichText::new("—").color(Theme::text_dim()))
                                        .selectable(false)
                                        .sense(Sense::hover()),
                                )
                            } else {
                                egui::ComboBox::from_id_salt("cont")
                                    .width(display_widths[7])
                                    .selected_text(cue.continue_mode.label())
                                    .show_ui(ui, |ui| {
                                        for mode in ContinueMode::ALL {
                                            ui.selectable_value(
                                                &mut cue.continue_mode,
                                                mode.clone(),
                                                mode.label(),
                                            );
                                        }
                                    })
                                    .response
                            }
                        });
                        continue_cell_rect = continue_response.response.rect;
                        ui.spacing_mut().item_spacing.x = 8.0;
                    });

                    let pointer_position = pointer_pos;
                    let pointer_clicked_in_row = ui
                        .input(|input| input.pointer.button_clicked(PointerButton::Primary))
                        && pointer_position
                            .map(|position| row_rect.contains(position))
                            .unwrap_or(false);
                    // A ComboBox popup is rendered on its own foreground layer.
                    // Do not use the row's manual pointer fallback when the click
                    // belongs to that popup, otherwise selecting a Continue mode
                    // also selects the row underneath it.
                    let click_is_on_row_layer = pointer_position
                        .and_then(|position| ui.ctx().layer_id_at(position))
                        .map(|layer| layer == ui.layer_id())
                        .unwrap_or(false);
                    let clicked_continue_control = pointer_position
                        .map(|position| continue_cell_rect.contains(position))
                        .unwrap_or(false);
                    if (row_resp.clicked() || (pointer_clicked_in_row && click_is_on_row_layer))
                        && !clicked_continue_control
                    {
                        let modifiers = ui.input(|input| input.modifiers);
                        Self::apply_selection(
                            cue.id,
                            &display_ids,
                            selected,
                            selected_ids,
                            selection_anchor,
                            modifiers,
                        );
                    }

                    let row_double_clicked = ui
                        .input(|input| input.pointer.button_double_clicked(PointerButton::Primary))
                        && click_is_on_row_layer
                        && pointer_position
                            .map(|position| row_rect.contains(position))
                            .unwrap_or(false);
                    if row_resp.double_clicked() || row_double_clicked {
                        if let Some(position) = pointer_position {
                            if cue_number_rect.contains(position) {
                                action = Some(CueTableAction::StartEdit(
                                    cue.id,
                                    EditField::Number,
                                    cue.number.clone(),
                                ));
                            } else if name_cell_rect.contains(position) {
                                action = Some(CueTableAction::StartEdit(
                                    cue.id,
                                    EditField::Name,
                                    cue.name.clone(),
                                ));
                            } else if pre_wait_rect.contains(position) {
                                action = Some(CueTableAction::StartEdit(
                                    cue.id,
                                    EditField::PreWait,
                                    format!("{:.2}", cue.pre_wait_secs),
                                ));
                            } else if post_wait_rect.contains(position) {
                                action = Some(CueTableAction::StartEdit(
                                    cue.id,
                                    EditField::PostWait,
                                    format!("{:.2}", cue.post_wait_secs),
                                ));
                            } else if duration_cell_rect.contains(position)
                                && matches!(&cue.cue_type, CueType::Fade { .. })
                            {
                                action = Some(CueTableAction::StartEdit(
                                    cue.id,
                                    EditField::Duration,
                                    format!("{:.2}", cue.effective_duration()),
                                ));
                            }
                        }
                    }

                    if drag_state
                        .as_ref()
                        .map(|state| state.active)
                        .unwrap_or(false)
                    {
                        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                        if let Some(position) =
                            pointer_pos.filter(|position| row_rect.contains(*position))
                        {
                            if !active_drag_ids.contains(&cue.id) {
                                let (parent_id, sibling_index) =
                                    sibling_locations.get(&cue.id).copied().unwrap_or((None, 0));
                                if is_group_row {
                                    // Only the central band of a group row means
                                    // "drop into group". Its upper/lower bands are
                                    // explicit before/after insertion zones.
                                    let relative_y = position.y - row_rect.top();
                                    let group_band = row_h * 0.28;
                                    if relative_y >= group_band && relative_y <= row_h - group_band
                                    {
                                        let child_count =
                                            group_child_counts.get(&cue.id).copied().unwrap_or(0);
                                        drop_destination = Some((Some(cue.id), child_count));
                                        drop_line_y = None;
                                        ui.painter().rect_stroke(
                                            row_rect,
                                            2.0_f32,
                                            Stroke::new(2.0_f32, Theme::accent2()),
                                            StrokeKind::Inside,
                                        );
                                    } else {
                                        let before = relative_y < row_h * 0.5;
                                        drop_destination = Some((
                                            parent_id,
                                            sibling_index + (if before { 0 } else { 1 }),
                                        ));
                                        drop_line_y = Some(if before {
                                            row_rect.top()
                                        } else {
                                            row_rect.bottom()
                                        });
                                    }
                                } else {
                                    let before = position.y < row_rect.center().y;
                                    drop_destination = Some((
                                        parent_id,
                                        sibling_index + (if before { 0 } else { 1 }),
                                    ));
                                    drop_line_y = Some(if before {
                                        row_rect.top()
                                    } else {
                                        row_rect.bottom()
                                    });
                                }
                            }
                        }

                        let primary_dragged_id = drag_state.as_ref().map(|state| state.source_id);
                        let is_targetable = matches!(
                            &cue.cue_type,
                            CueType::Play { .. }
                                | CueType::Pause { .. }
                                | CueType::Stop { .. }
                                | CueType::Fade { .. }
                        );
                        let pointer_over_target = pointer_pos
                            .map(|position| target_cell_rect.contains(position))
                            .unwrap_or(false);
                        let released_over_target =
                            ui.input(|input| input.pointer.button_released(PointerButton::Primary));
                        if let Some(pid) = primary_dragged_id {
                            let valid_target =
                                pid != cue.id && is_targetable && targetable_ids.contains(&pid);
                            if pointer_over_target && valid_target {
                                ui.painter().rect_stroke(
                                    target_cell_rect,
                                    2.0,
                                    Stroke::new(2.0, Theme::accent2()),
                                    StrokeKind::Inside,
                                );
                                if released_over_target {
                                    action = Some(CueTableAction::AssignTarget(pid, cue.id));
                                    drag_state = None;
                                    drop_destination = None;
                                    drop_line_y = None;
                                }
                            }
                        }
                    }

                    // Контекстное меню
                    row_resp.context_menu(|ui| {
                        if ui.button("▶  Play / Trigger").clicked() {
                            action = Some(CueTableAction::Play(cue.id));
                            ui.close_menu();
                        }
                        if ui.button("⏸  Pause").clicked() {
                            action = Some(CueTableAction::Pause(cue.id));
                            ui.close_menu();
                        }
                        if ui.button("⏹  Stop").clicked() {
                            action = Some(CueTableAction::Stop(cue.id));
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("⬆  Move Up").clicked() {
                            action = Some(CueTableAction::MoveUp(cue.id));
                            ui.close_menu();
                        }
                        if ui.button("⬇  Move Down").clicked() {
                            action = Some(CueTableAction::MoveDown(cue.id));
                            ui.close_menu();
                        }
                        if ui.button("Clear cue number").clicked() {
                            action = Some(CueTableAction::ClearNumber(cue.id));
                            ui.close_menu();
                        }
                        if ui.button("✏  Rename").clicked() {
                            action = Some(CueTableAction::StartEdit(
                                cue.id,
                                EditField::Name,
                                cue.name.clone(),
                            ));
                            ui.close_menu();
                        }
                        if ui.button("🗑  Delete").clicked() {
                            action = Some(CueTableAction::Delete(cue.id));
                            ui.close_menu();
                        }
                        ui.separator();
                        ui.menu_button("🎨  Color", |ui| {
                            for color in CueColor::ALL {
                                if ui
                                    .add(Button::new(
                                        RichText::new(color.label()).color(color.to_egui()),
                                    ))
                                    .clicked()
                                {
                                    action = Some(CueTableAction::SetColor(cue.id, *color));
                                    ui.close_menu();
                                }
                            }
                        });
                    });

                    if missing_file {
                        let tooltip_response = ui.interact(
                            row_rect,
                            Id::new(("missing_file_tooltip", cue.id)),
                            Sense::hover(),
                        );
                        let missing_path = if cue.file_path.as_os_str().is_empty() {
                            "No file path assigned".to_string()
                        } else {
                            cue.file_path.display().to_string()
                        };
                        tooltip_response
                            .on_hover_text(format!("Missing media file: {missing_path}"));
                    }

                    ui.add_space(1.0);
                }
                while let Some((_, rect)) = group_frame_stack.pop() {
                    completed_group_frames.push(rect);
                }
                for rect in completed_group_frames {
                    // A clear outline encloses the group header and every
                    // visible descendant. Collapsed groups retain a frame
                    // around their header row so they remain recognizable.
                    let outline = rect.expand2(vec2(3.0, 2.0));
                    ui.painter().rect_stroke(
                        outline,
                        5.0,
                        Stroke::new(2.0, Theme::accent2()),
                        StrokeKind::Inside,
                    );
                    ui.painter().line_segment(
                        [
                            pos2(outline.left() + 1.0, outline.top() + 5.0),
                            pos2(outline.left() + 1.0, outline.bottom() - 5.0),
                        ],
                        Stroke::new(3.0, Theme::accent()),
                    );
                }

                // A real end-of-list drop target lives inside the vertical
                // scroll content. If the final visible cue is a group child,
                // the drop remains in that group's sibling sequence.
                let tail_height = (viewport_rect.bottom() - ui.cursor().min.y).max(row_h);
                let (tail_rect, _) =
                    ui.allocate_exact_size(vec2(table_width, tail_height), Sense::hover());
                if drag_state
                    .as_ref()
                    .map(|state| state.active)
                    .unwrap_or(false)
                    && ui
                        .ctx()
                        .pointer_latest_pos()
                        .map(|position| tail_rect.contains(position))
                        .unwrap_or(false)
                {
                    if let Some(last_id) = display_ids.last().copied() {
                        let (parent_id, sibling_index) = sibling_locations
                            .get(&last_id)
                            .copied()
                            .unwrap_or((None, 0));
                        drop_destination = Some((parent_id, sibling_index + 1));
                    } else {
                        drop_destination = Some((None, 0));
                    }
                    drop_line_y = Some(tail_rect.top() + 2.0);
                }

                if let Some(y) = drop_line_y {
                    let left = viewport_rect.left();
                    let right = left + table_width;
                    let mut x = left;
                    while x < right {
                        let end_x = (x + 7.0).min(right);
                        ui.painter().line_segment(
                            [pos2(x, y), pos2(end_x, y)],
                            Stroke::new(2.0, Theme::accent2()),
                        );
                        x += 12.0;
                    }
                }
            });

        if let Some(state) = drag_state.as_ref().filter(|state| state.active) {
            if let Some(pos) = ui.ctx().pointer_latest_pos() {
                let label = if state.dragged_ids.len() > 1 {
                    format!("{} selected events", state.dragged_ids.len())
                } else if let Some(dragged_cue) = cues.iter().find(|cue| cue.id == state.source_id)
                {
                    if dragged_cue.number.trim().is_empty() {
                        dragged_cue.name.clone()
                    } else {
                        format!("#{} {}", dragged_cue.number, dragged_cue.name)
                    }
                } else {
                    "Moving event".to_string()
                };
                let ghost_rect = Rect::from_center_size(pos, vec2(300.0, 24.0));
                ui.painter().rect_filled(
                    ghost_rect,
                    4.0,
                    Color32::from_rgba_unmultiplied(90, 170, 255, 180),
                );
                ui.painter().text(
                    ghost_rect.center(),
                    Align2::CENTER_CENTER,
                    label,
                    FontId::proportional(12.0),
                    Color32::WHITE,
                );
            }
        }

        // Keep the drag candidate and full selection across frames. On release,
        // emit exactly one move action and clear the temporary state.
        if ui.input(|input| input.pointer.any_released()) {
            if let Some(state) = drag_state.take().filter(|state| state.active) {
                if action.is_none() {
                    if let Some((parent_id, sibling_index)) = drop_destination {
                        action = Some(CueTableAction::MoveMultipleToPosition(
                            state.dragged_ids,
                            parent_id,
                            sibling_index,
                        ));
                    }
                }
            }
            ui.data_mut(|data| data.insert_temp(drag_state_id, None::<CueDragState>));
        } else {
            ui.data_mut(|data| data.insert_temp(drag_state_id, drag_state.clone()));
        }

        // Clicking unused table space clears every selection. Row/header widgets
        // are excluded, so normal clicks and column resizing keep their behaviour.
        let primary_clicked =
            ui.input(|input| input.pointer.button_clicked(PointerButton::Primary));
        if primary_clicked {
            if let Some(position) = ui.ctx().pointer_latest_pos() {
                let table_area_rect = Rect::from_min_size(
                    viewport_rect.min,
                    vec2(table_width, viewport_rect.height()),
                );
                let inside_table = table_area_rect.contains(position);
                let on_header = header_rect.contains(position);
                let on_row = row_rects.iter().any(|rect| rect.contains(position));
                if inside_table && !on_header && !on_row && action.is_none() {
                    action = Some(CueTableAction::Deselect);
                }
            }
        }

        action
    }

    fn apply_selection(
        cue_id: Uuid,
        visible_ids: &[Uuid],
        selected: &mut Option<Uuid>,
        selected_ids: &mut HashSet<Uuid>,
        anchor: &mut Option<Uuid>,
        modifiers: Modifiers,
    ) {
        if modifiers.shift {
            let anchor_id = (*anchor).or(*selected).unwrap_or(cue_id);
            if anchor.is_none() {
                *anchor = Some(anchor_id);
            }
            if let (Some(anchor_index), Some(target_index)) = (
                visible_ids.iter().position(|id| *id == anchor_id),
                visible_ids.iter().position(|id| *id == cue_id),
            ) {
                selected_ids.clear();
                let start = anchor_index.min(target_index);
                let end = anchor_index.max(target_index);
                selected_ids.extend(visible_ids[start..=end].iter().copied());
            } else {
                selected_ids.insert(cue_id);
            }
            *selected = Some(cue_id);
        } else if modifiers.ctrl {
            if !selected_ids.remove(&cue_id) {
                selected_ids.insert(cue_id);
            }
            if selected_ids.contains(&cue_id) {
                *selected = Some(cue_id);
            } else {
                *selected = selected_ids.iter().next().copied();
            }
            *anchor = Some(cue_id);
        } else {
            selected_ids.clear();
            selected_ids.insert(cue_id);
            *selected = Some(cue_id);
            *anchor = Some(cue_id);
        }
    }

    /// Ячейка Pre-Wait / Post-Wait с редактированием по двойному клику.
    fn wait_cell(
        ui: &mut Ui,
        cue_id: Uuid,
        field: EditField,
        value: f64,
        width: f32,
        row_h: f32,
        accent: Color32,
        phase: WaitPhase,
        active_phase: WaitPhase,
        wait_started_at: Option<Instant>,
        wait_duration_secs: f64,
        editing: &mut Option<(Uuid, EditField, String)>,
        editing_focus_pending: &mut bool,
        action: &mut Option<CueTableAction>,
        _indent: u8,
    ) -> Rect {
        let is_editing = matches!(editing, Some((id, f, _)) if *id == cue_id && *f == field);
        if is_editing {
            if let Some((_, _, ref mut buf)) = editing {
                let response = ui.add_sized(
                    [width, row_h - 4.0],
                    TextEdit::singleline(buf).font(TextStyle::Monospace),
                );
                if *editing_focus_pending {
                    response.request_focus();
                    *editing_focus_pending = false;
                }
                if response.lost_focus() || ui.input(|i| i.key_pressed(Key::Enter)) {
                    *action = Some(CueTableAction::EditConfirm(cue_id, field, buf.clone()));
                }
                if ui.input(|i| i.key_pressed(Key::Escape)) {
                    *action = Some(CueTableAction::EditCancel);
                }
                response.rect
            } else {
                Rect::NOTHING
            }
        } else if active_phase == phase && wait_duration_secs > 0.0 {
            if let Some(started_at) =
                wait_started_at.filter(|started_at| Instant::now() >= *started_at)
            {
                let now = Instant::now();
                let elapsed = now
                    .duration_since(started_at)
                    .as_secs_f64()
                    .min(wait_duration_secs);
                let progress = (elapsed / wait_duration_secs.max(0.001)).clamp(0.0, 1.0) as f32;
                let (rect, _) = ui.allocate_exact_size(vec2(width, row_h - 8.0), Sense::hover());
                let bar_rect =
                    Rect::from_min_size(rect.min, vec2(rect.width() * progress, rect.height()));
                ui.painter()
                    .rect_filled(bar_rect, 2.0_f32, accent.linear_multiply(0.28_f32));
                ui.painter().text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    if wait_duration_secs - elapsed <= 0.0 {
                        "00:00.00".to_string()
                    } else {
                        format_duration(wait_duration_secs - elapsed)
                    },
                    FontId::monospace(10.0),
                    accent,
                );
                rect
            } else {
                ui.add_sized(
                    [width, row_h],
                    Label::new(
                        RichText::new(format_duration(wait_duration_secs))
                            .color(accent)
                            .monospace()
                            .size(11.0),
                    )
                    .selectable(false)
                    .sense(Sense::hover()),
                )
                .rect
            }
        } else {
            let txt = if value > 0.0 {
                format_duration(value)
            } else {
                "00:00.00".to_string()
            };
            ui.add_sized(
                [width, row_h],
                Label::new(
                    RichText::new(txt)
                        .color(if value > 0.0 {
                            accent
                        } else {
                            Theme::text_dim()
                        })
                        .monospace()
                        .size(11.0),
                )
                .selectable(false)
                .sense(Sense::hover()),
            )
            .rect
        }
    }

    fn describe_target(cue: &Cue, cue_names: &std::collections::HashMap<Uuid, String>) -> String {
        match &cue.cue_type {
            CueType::Audio => cue
                .file_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("—")
                .to_string(),
            CueType::Play { target_id }
            | CueType::Pause { target_id }
            | CueType::Stop { target_id }
            | CueType::Fade { target_id, .. } => {
                if let Some(tid) = target_id {
                    cue_names
                        .get(tid)
                        .cloned()
                        .unwrap_or_else(|| "Missing target".to_string())
                } else if !cue.file_path.as_os_str().is_empty() {
                    cue.file_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("—")
                        .to_string()
                } else {
                    "All / None".to_string()
                }
            }
            CueType::Midi {
                channel, message, ..
            } => format!("Ch {} • {}", channel, message.label()),
            CueType::Osc { address, .. } => address.clone(),
            CueType::Comment { text } => text
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("Comment")
                .to_string(),
            CueType::Group { mode, children, .. } => {
                format!("{} ({} cues)", mode.label(), children.len())
            }
        }
    }

    fn row_bg(cue: &Cue, idx: usize) -> Color32 {
        match cue.state {
            CueState::Paused | CueState::Waiting => Theme::bg_paused(),
            _ => {
                if idx % 2 == 0 {
                    Theme::bg_row()
                } else {
                    Theme::bg_row_alt()
                }
            }
        }
    }

    fn name_color(cue: &Cue) -> Color32 {
        match cue.state {
            CueState::Playing | CueState::FadingIn => Theme::text_active(),
            CueState::Paused | CueState::Waiting => Theme::pause_btn(),
            _ => Theme::text(),
        }
    }
}

pub enum CueTableAction {
    Deselect,
    Play(Uuid),
    Stop(Uuid),
    Pause(Uuid),
    Delete(Uuid),
    MoveUp(Uuid),
    MoveDown(Uuid),
    MoveMultipleToPosition(Vec<Uuid>, Option<Uuid>, usize),
    StartEdit(Uuid, EditField, String),
    ClearNumber(Uuid),
    EditConfirm(Uuid, EditField, String),
    EditCancel,
    SetColor(Uuid, CueColor),
    AssignTarget(Uuid, Uuid),
}

fn status_label(state: &CueState) -> (&'static str, Color32) {
    match state {
        CueState::Stopped => ("stopped", Theme::text_dim()),
        CueState::Waiting => ("⏳ waiting", Theme::pause_btn()),
        CueState::Playing => ("▶ playing", Theme::text_active()),
        CueState::Paused => ("⏸ paused", Theme::pause_btn()),
        CueState::FadingIn => ("↗ fade in", Theme::accent2()),
        CueState::FadingOut => ("↘ fade out", Theme::accent()),
        CueState::Loading => ("loading…", Theme::accent()),
        CueState::Error(_) => ("⚠ error", Theme::stop_btn()),
    }
}

pub fn format_duration(secs: f64) -> String {
    if secs <= 0.0 {
        return "--:--".to_string();
    }
    let m = (secs / 60.0) as u32;
    let s = secs as u32 % 60;
    let cs = ((secs.fract()) * 100.0) as u32;
    format!("{m:02}:{s:02}.{cs:02}")
}

#[cfg(test)]
mod tests {
    use super::{build_sibling_locations, visible_cue_ids, visible_navigation_cue_ids};
    use crate::cue::{Cue, CueType, GroupMode};
    use std::collections::HashSet;

    #[test]
    fn visible_navigation_uses_group_child_order_not_cue_number_order() {
        let child_a = Cue::new_typed(CueType::Stop { target_id: None });
        let child_b = Cue::new_typed(CueType::Pause { target_id: None });
        let group = Cue::new_typed(CueType::Group {
            mode: GroupMode::Normal,
            children: vec![child_b.id, child_a.id],
            fade_in_secs: 0.0,
            fade_out_secs: 0.0,
        });
        let after = Cue::new_typed(CueType::Midi {
            channel: 1,
            message: crate::cue::MidiMessageKind::NoteOn {
                note: 60,
                velocity: 100,
            },
            raw_hex: String::new(),
        });
        let expected = vec![group.id, child_b.id, child_a.id, after.id];
        let cues = vec![
            group.clone(),
            after.clone(),
            child_a.clone(),
            child_b.clone(),
        ];
        let mut expanded = HashSet::new();
        expanded.insert(group.id);
        assert_eq!(visible_cue_ids(&cues, &expanded), expected);
    }

    #[test]
    fn comment_rows_remain_visible_but_are_skipped_by_keyboard_navigation() {
        let before = Cue::new_typed(CueType::Stop { target_id: None });
        let comment = Cue::new_typed(CueType::Comment {
            text: "Lights to standby".to_string(),
        });
        let after = Cue::new_typed(CueType::Pause { target_id: None });
        let cues = vec![before.clone(), comment.clone(), after.clone()];

        assert_eq!(
            visible_cue_ids(&cues, &HashSet::new()),
            vec![before.id, comment.id, after.id]
        );
        assert_eq!(
            visible_navigation_cue_ids(&cues, &HashSet::new()),
            vec![before.id, after.id]
        );
    }

    #[test]
    fn collapsed_groups_hide_children_from_keyboard_navigation() {
        let child = Cue::new_typed(CueType::Stop { target_id: None });
        let group = Cue::new_typed(CueType::Group {
            mode: GroupMode::Normal,
            children: vec![child.id],
            fade_in_secs: 0.0,
            fade_out_secs: 0.0,
        });
        let after = Cue::new_typed(CueType::Pause { target_id: None });
        let cues = vec![group.clone(), after.clone(), child];
        assert_eq!(
            visible_cue_ids(&cues, &HashSet::new()),
            vec![group.id, after.id]
        );
    }

    #[test]
    fn sibling_locations_separate_root_rows_from_group_children() {
        let first_root = Cue::new_typed(CueType::Stop { target_id: None });
        let child_a = Cue::new_typed(CueType::Pause { target_id: None });
        let child_b = Cue::new_typed(CueType::Play { target_id: None });
        let group = Cue::new_typed(CueType::Group {
            mode: GroupMode::Normal,
            children: vec![child_a.id, child_b.id],
            fade_in_secs: 0.0,
            fade_out_secs: 0.0,
        });
        let last_root = Cue::new_typed(CueType::Stop { target_id: None });
        let cues = vec![
            first_root.clone(),
            group.clone(),
            child_a.clone(),
            child_b.clone(),
            last_root.clone(),
        ];

        let locations = build_sibling_locations(&cues);
        assert_eq!(locations.get(&first_root.id), Some(&(None, 0)));
        assert_eq!(locations.get(&group.id), Some(&(None, 1)));
        assert_eq!(locations.get(&child_a.id), Some(&(Some(group.id), 0)));
        assert_eq!(locations.get(&child_b.id), Some(&(Some(group.id), 1)));
        assert_eq!(locations.get(&last_root.id), Some(&(None, 2)));
    }

    #[test]
    fn collapsed_parent_hides_expanded_nested_group_children() {
        let nested_child = Cue::new_typed(CueType::Stop { target_id: None });
        let nested_group = Cue::new_typed(CueType::Group {
            mode: GroupMode::Normal,
            children: vec![nested_child.id],
            fade_in_secs: 0.0,
            fade_out_secs: 0.0,
        });
        let parent = Cue::new_typed(CueType::Group {
            mode: GroupMode::Normal,
            children: vec![nested_group.id],
            fade_in_secs: 0.0,
            fade_out_secs: 0.0,
        });
        let cues = vec![parent.clone(), nested_group.clone(), nested_child.clone()];
        let mut expanded = HashSet::new();
        // The nested group may remain marked expanded while its parent is closed.
        expanded.insert(nested_group.id);
        assert_eq!(visible_cue_ids(&cues, &expanded), vec![parent.id]);
    }
}
