// src/ui/cue_table.rs
#![allow(float_literal_f32_fallback)]
use crate::cue::{ContinueMode, Cue, CueColor, CueState, CueType};
use crate::ui::theme::Theme;
use egui::*;
use std::collections::HashSet;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditField {
    Name,
    PreWait,
    PostWait,
}

pub struct CueTable;

impl CueTable {
    pub fn show(
        ui: &mut Ui,
        cues: &mut Vec<Cue>,
        selected: &mut Option<Uuid>,
        editing: &mut Option<(Uuid, EditField, String)>,
        expanded_groups: &mut HashSet<Uuid>,
    ) -> Option<CueTableAction> {
        let mut action = None;

        let cue_names: std::collections::HashMap<Uuid, String> = cues
            .iter()
            .map(|c| (c.id, format!("#{} {}", c.number, c.name)))
            .collect();

        // Множество id, входящих в какую-либо группу (чтобы скрыть с верхнего уровня)
        let child_ids: HashSet<Uuid> = cues
            .iter()
            .filter_map(|c| match &c.cue_type {
                CueType::Group { children, .. } => Some(children.iter().copied()),
                _ => None,
            })
            .flatten()
            .collect();

        let row_h = 28.0_f32;
        let col_widths = [44.0_f32, 75.0, 210.0, 165.0, 68.0, 78.0, 68.0, 95.0, 125.0];

        // Заголовок
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            for (label, w) in [
                ("Cue", col_widths[0]),
                ("Type", col_widths[1]),
                ("Name", col_widths[2]),
                ("File / Target", col_widths[3]),
                ("Pre-Wait", col_widths[4]),
                ("Duration", col_widths[5]),
                ("Post-Wait", col_widths[6]),
                ("Status", col_widths[7]),
                ("Continue", col_widths[8]),
            ] {
                ui.add_sized(
                    [w, 20.0],
                    Label::new(RichText::new(label).color(Theme::TEXT_DIM).size(11.0)),
                );
            }
        });
        ui.add(Separator::default().horizontal());

        // Предвычисляем порядок отображения: (индекс в cues, уровень отступа)
        let mut display_order: Vec<(usize, u8)> = Vec::new();
        for (idx, cue) in cues.iter().enumerate() {
            if child_ids.contains(&cue.id) {
                continue;
            }
            display_order.push((idx, 0));
            if let CueType::Group { children, .. } = &cue.cue_type {
                if expanded_groups.contains(&cue.id) {
                    for child_id in children {
                        if let Some(cidx) = cues.iter().position(|c| c.id == *child_id) {
                            display_order.push((cidx, 1));
                        }
                    }
                }
            }
        }

        let mut dropped_target_idx: Option<usize> = None;
        // id того, что тащат — берём из egui memory
        let dragged_id = ui.data(|d| d.get_temp::<Uuid>(Id::new("dragged_cue_id")));

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for &(cue_idx, indent) in &display_order {
                    let cue = &mut cues[cue_idx];
                    let is_selected = *selected == Some(cue.id);
                    let row_start = ui.cursor().min;
                    let row_rect =
                        Rect::from_min_size(row_start, vec2(ui.available_width(), row_h));

                    // Фон
                    let base_bg = Self::row_bg(cue, is_selected, cue_idx);
                    ui.painter().rect_filled(row_rect, 0.0_f32, base_bg);

                    // Тонировка цветом (теперь с alpha=26)
                    if let Some(cue_tint) = cue.color.row_bg_egui() {
                        ui.painter().rect_filled(row_rect, 0.0_f32, cue_tint);
                    }

                    // Визуальная подсветка перетаскиваемого
                    if dragged_id == Some(cue.id) {
                        ui.painter().rect_filled(
                            row_rect,
                            2.0_f32,
                            Color32::from_rgba_unmultiplied(90, 170, 255, 55),
                        );
                        ui.painter().rect_stroke(
                            row_rect,
                            2.0_f32,
                            Stroke::new(1.5_f32, Theme::ACCENT),
                            StrokeKind::Inside,
                        );
                    }

                    if is_selected {
                        ui.painter().rect_stroke(
                            row_rect,
                            2.0_f32,
                            Stroke::new(1.5_f32, Theme::ACCENT),
                            StrokeKind::Inside,
                        );
                    }

                    let resp = ui.horizontal(|ui| {
                        // Отступ для детей группы
                        if indent > 0 {
                            ui.add_space(indent as f32 * 16.0);
                        }

                        // Стрелка раскрытия группы (только для групп, только верхний уровень)
                        let is_group = matches!(&cue.cue_type, CueType::Group { .. });
                        if is_group && indent == 0 {
                            let open = expanded_groups.contains(&cue.id);
                            let arrow = if open { "▼" } else { "▶" };
                            let btn = ui.add_sized(
                                [18.0, row_h - 6.0],
                                Button::new(RichText::new(arrow).size(11.0))
                                    .frame(false)
                                    .fill(Color32::TRANSPARENT),
                            );
                            if btn.clicked() {
                                let id = cue.id;
                                if open {
                                    expanded_groups.remove(&id);
                                } else {
                                    expanded_groups.insert(id);
                                }
                            }
                        } else {
                            ui.add_space(18.0);
                        }

                        // Цветная полоска слева
                        let (strip_rect, _) =
                            ui.allocate_exact_size(vec2(5.0, row_h), Sense::hover());
                        ui.painter()
                            .rect_filled(strip_rect, 0.0_f32, cue.color.to_egui());
                        ui.add_space(3.0);

                        // Cue #
                        ui.add_sized(
                            [col_widths[0], row_h],
                            Label::new(
                                RichText::new(&cue.number)
                                    .color(Theme::ACCENT)
                                    .monospace()
                                    .size(12.0),
                            ),
                        );

                        // Type
                        ui.add_sized(
                            [col_widths[1], row_h],
                            Label::new(
                                RichText::new(format!(
                                    "{} {}",
                                    cue.cue_type.icon(),
                                    cue.cue_type.label()
                                ))
                                .color(Theme::TEXT)
                                .size(11.5),
                            ),
                        );

                        // Name — двойной клик по имени запускает редактирование
                        let is_editing_name =
                            matches!(editing, Some((id, EditField::Name, _)) if *id == cue.id);
                        if is_editing_name {
                            if let Some((_, _, ref mut buf)) = editing {
                                let r = ui.add_sized(
                                    [col_widths[2], row_h - 4.0],
                                    TextEdit::singleline(buf).font(TextStyle::Body),
                                );
                                r.request_focus();
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
                            let name_label = ui.add_sized(
                                [col_widths[2], row_h],
                                Label::new(
                                    RichText::new(&cue.name)
                                        .color(Self::name_color(cue))
                                        .size(13.0)
                                        .strong(),
                                )
                                .truncate()
                                .sense(Sense::click()),
                            );
                            if name_label.double_clicked() {
                                action = Some(CueTableAction::StartEdit(
                                    cue.id,
                                    EditField::Name,
                                    cue.name.clone(),
                                ));
                            } else if name_label.clicked() {
                                *selected = Some(cue.id);
                            }
                        }

                        // File / Target
                        let target_desc = Self::describe_target(cue, &cue_names);
                        ui.add_sized(
                            [col_widths[3], row_h],
                            Label::new(
                                RichText::new(target_desc).color(Theme::TEXT_DIM).size(11.0),
                            )
                            .truncate(),
                        );

                        // Pre-Wait (двойной клик = редактирование)
                        Self::wait_cell(
                            ui,
                            cue.id,
                            EditField::PreWait,
                            cue.pre_wait_secs,
                            col_widths[4],
                            row_h,
                            Theme::ACCENT,
                            editing,
                            &mut action,
                            indent,
                        );

                        // Duration
                        ui.add_sized(
                            [col_widths[5], row_h],
                            Label::new(
                                RichText::new(format_duration(cue.effective_duration()))
                                    .color(Theme::TEXT_DIM)
                                    .monospace()
                                    .size(11.0),
                            ),
                        );

                        // Post-Wait
                        Self::wait_cell(
                            ui,
                            cue.id,
                            EditField::PostWait,
                            cue.post_wait_secs,
                            col_widths[6],
                            row_h,
                            Theme::ACCENT2,
                            editing,
                            &mut action,
                            indent,
                        );

                        // Status
                        ui.add_sized([col_widths[7], row_h], |ui: &mut Ui| {
                            let (rect, resp) = ui.allocate_exact_size(
                                vec2(col_widths[7], row_h - 8.0),
                                Sense::hover(),
                            );
                            if cue.is_active() && cue.effective_duration() > 0.0 {
                                let progress = cue.progress();
                                let bar_rect = Rect::from_min_size(
                                    rect.min,
                                    vec2(rect.width() * progress, rect.height()),
                                );
                                ui.painter().rect_filled(
                                    bar_rect,
                                    2.0_f32,
                                    Theme::BG_ACTIVE.linear_multiply(0.8_f32),
                                );
                                ui.painter().text(
                                    rect.center(),
                                    Align2::CENTER_CENTER,
                                    format_duration(cue.remaining_secs()),
                                    FontId::monospace(10.0),
                                    Theme::TEXT_ACTIVE,
                                );
                            } else {
                                let (label, color) = status_label(&cue.state);
                                ui.painter().text(
                                    rect.center(),
                                    Align2::CENTER_CENTER,
                                    label,
                                    FontId::proportional(11.0),
                                    color,
                                );
                            }
                            resp
                        });

                        // Continue
                        ui.push_id(("continue_combo", cue.id), |ui| {
                            egui::ComboBox::from_id_salt("cont")
                                .width(col_widths[8] - 8.0)
                                .selected_text(cue.continue_mode.label())
                                .show_ui(ui, |ui| {
                                    for mode in ContinueMode::ALL {
                                        ui.selectable_value(
                                            &mut cue.continue_mode,
                                            mode.clone(),
                                            mode.label(),
                                        );
                                    }
                                });
                        });
                    });

                    let is_group = matches!(&cue.cue_type, CueType::Group { .. });
                    let left_offset = if is_group && indent == 0 { 18.0 } else { 0.0 };
                    let interact_rect = Rect::from_min_max(
                        pos2(
                            resp.response.rect.left() + left_offset,
                            resp.response.rect.top(),
                        ),
                        pos2(
                            resp.response.rect.left()
                                + left_offset
                                + col_widths[0]
                                + col_widths[1]
                                + 30.0,
                            resp.response.rect.bottom(),
                        ),
                    );
                    let row_resp = ui.interact(
                        interact_rect,
                        Id::new(("cue_row_click", cue.id)),
                        Sense::click_and_drag(),
                    );

                    if row_resp.drag_started() {
                        ui.data_mut(|d| d.insert_temp(Id::new("dragged_cue_id"), cue.id));
                        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                    }
                    if row_resp.dragged() {
                        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                    }
                    if row_resp.drag_stopped() {
                        ui.data_mut(|d| {
                            d.insert_temp(
                                Id::new("drag_hold_target"),
                                (Uuid::nil(), std::time::Instant::now()),
                            );
                        });
                    }

                    // Подсветка цели при наведении
                    let currently_dragged =
                        ui.data(|d| d.get_temp::<Uuid>(Id::new("dragged_cue_id")));
                    if let Some(pid) = currently_dragged {
                        if row_resp.hovered() && pid != cue.id {
                            dropped_target_idx = Some(cue_idx);
                            ui.painter().rect_stroke(
                                row_rect,
                                2.0_f32,
                                Stroke::new(2.0_f32, Theme::ACCENT2),
                                StrokeKind::Inside,
                            );
                        }
                    }

                    if let Some(pid) = currently_dragged {
                        if row_resp.hovered() && pid != cue.id {
                            let is_targetable = matches!(
                                &cue.cue_type,
                                CueType::Play { .. }
                                    | CueType::Pause { .. }
                                    | CueType::Stop { .. }
                                    | CueType::Fade { .. }
                            );

                            if is_targetable {
                                // Сообщаем наверх, что пользователь «завис» над целью
                                let existing = ui.data(|d| {
                                    d.get_temp::<(Uuid, std::time::Instant)>(Id::new(
                                        "drag_hold_target",
                                    ))
                                });
                                match existing {
                                    Some((id, _)) if id == cue.id => {
                                        // таймер уже идёт — ничего не трогаем
                                    }
                                    _ => {
                                        ui.data_mut(|d| {
                                            d.insert_temp(
                                                Id::new("drag_hold_target"),
                                                (cue.id, std::time::Instant::now()),
                                            );
                                        });
                                    }
                                }

                                // Визуальная индикация: показываем, что цель активна
                                ui.painter().rect_stroke(
                                    row_rect,
                                    2.0,
                                    Stroke::new(2.0, Theme::ACCENT2),
                                    StrokeKind::Inside,
                                );

                                // Проверяем, прошло ли 3 секунды
                                if let Some((target_id, start_time)) = ui.data(|d| {
                                    d.get_temp::<(Uuid, std::time::Instant)>(Id::new(
                                        "drag_hold_target",
                                    ))
                                }) {
                                    if start_time.elapsed().as_secs_f32() >= 3.0
                                        && target_id == cue.id
                                    {
                                        action = Some(CueTableAction::AssignTarget(pid, cue.id));
                                        ui.data_mut(|d| {
                                            d.insert_temp(
                                                Id::new("drag_hold_target"),
                                                (Uuid::nil(), std::time::Instant::now()),
                                            );
                                        });
                                    }
                                }
                            }
                        } else {
                            if let Some((target_id, _)) = ui.data(|d| {
                                d.get_temp::<(Uuid, std::time::Instant)>(Id::new(
                                    "drag_hold_target",
                                ))
                            }) {
                                if target_id != cue.id {
                                    ui.data_mut(|d| {
                                        d.insert_temp(
                                            Id::new("drag_hold_target"),
                                            (Uuid::nil(), std::time::Instant::now()),
                                        );
                                    });
                                }
                            }
                        }
                    }

                    if row_resp.clicked() {
                        *selected = Some(cue.id);
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

                    ui.add_space(1.0);
                }
            });

        if let Some(drag_id) = dragged_id {
            if let Some(pos) = ui.ctx().pointer_latest_pos() {
                if let Some(dragged_cue) = cues.iter().find(|c| c.id == drag_id) {
                    let ghost_rect = Rect::from_center_size(pos, vec2(300.0, 24.0));
                    ui.painter().rect_filled(
                        ghost_rect,
                        4.0,
                        Color32::from_rgba_unmultiplied(90, 170, 255, 180),
                    );
                    ui.painter().text(
                        ghost_rect.center(),
                        Align2::CENTER_CENTER,
                        format!("#{} {}", dragged_cue.number, dragged_cue.name),
                        FontId::proportional(12.0),
                        Color32::WHITE,
                    );
                    // Сброс выделения по клику мимо строк
                    if ui.input(|i| i.pointer.any_click())
                        && !ui.ctx().is_pointer_over_area()
                        && selected.is_some()
                    {
                        let any_row_hovered = display_order.iter().any(|(idx, _)| {
                            let cue = &cues[*idx];
                            ui.ctx()
                                .read_response(Id::new(("cue_row_click", cue.id)))
                                .map(|r| r.hovered())
                                .unwrap_or(false)
                        });
                        if !any_row_hovered {
                            *selected = None;
                        }
                    }
                }
            }
        }

        // Обработка drop
        if let Some(target_idx) = dropped_target_idx {
            let dragged_id = ui.data_mut(|d| d.remove_temp::<Uuid>(Id::new("dragged_cue_id")));
            if let Some(dragged_id) = dragged_id {
                action = Some(CueTableAction::MoveToGroupOrIndex(dragged_id, target_idx));
            }
        } else {
            // Если драг завершился, но цели нет — сбрасываем
            ui.data_mut(|d| {
                let _ = d.remove_temp::<Uuid>(Id::new("dragged_cue_id"));
            });
        }

        action
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
        editing: &mut Option<(Uuid, EditField, String)>,
        action: &mut Option<CueTableAction>,
        _indent: u8,
    ) {
        let is_editing = matches!(editing, Some((id, f, _)) if *id == cue_id && *f == field);
        if is_editing {
            if let Some((_, _, ref mut buf)) = editing {
                let r = ui.add_sized(
                    [width, row_h - 4.0],
                    TextEdit::singleline(buf).font(TextStyle::Monospace),
                );
                r.request_focus();
                if r.lost_focus() || ui.input(|i| i.key_pressed(Key::Enter)) {
                    *action = Some(CueTableAction::EditConfirm(cue_id, field, buf.clone()));
                }
                if ui.input(|i| i.key_pressed(Key::Escape)) {
                    *action = Some(CueTableAction::EditCancel);
                }
            }
        } else {
            let txt = if value > 0.0 {
                format_duration(value)
            } else {
                "00:00.00".to_string()
            };
            let label = ui.add_sized(
                [width, row_h],
                Label::new(
                    RichText::new(txt)
                        .color(if value > 0.0 { accent } else { Theme::TEXT_DIM })
                        .monospace()
                        .size(11.0),
                )
                .sense(Sense::click()),
            );
            if label.double_clicked() {
                *action = Some(CueTableAction::StartEdit(
                    cue_id,
                    field,
                    format!("{:.2}", value),
                ));
            }
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
            CueType::Group { mode, children, .. } => {
                format!("{} ({} cues)", mode.label(), children.len())
            }
        }
    }

    fn row_bg(cue: &Cue, selected: bool, idx: usize) -> Color32 {
        if selected {
            return Theme::BG_SELECTED;
        }
        match cue.state {
            CueState::Playing | CueState::FadingIn | CueState::FadingOut => Theme::BG_ACTIVE,
            CueState::Paused | CueState::Waiting => Theme::BG_PAUSED,
            _ => {
                if idx % 2 == 0 {
                    Theme::BG_ROW
                } else {
                    Theme::BG_ROW_ALT
                }
            }
        }
    }

    fn name_color(cue: &Cue) -> Color32 {
        match cue.state {
            CueState::Playing | CueState::FadingIn => Theme::TEXT_ACTIVE,
            CueState::Paused | CueState::Waiting => Theme::PAUSE_BTN,
            _ => Theme::TEXT,
        }
    }
}

pub enum CueTableAction {
    Play(Uuid),
    Stop(Uuid),
    Pause(Uuid),
    Delete(Uuid),
    MoveUp(Uuid),
    MoveDown(Uuid),
    MoveToGroupOrIndex(Uuid, usize),
    StartEdit(Uuid, EditField, String),
    EditConfirm(Uuid, EditField, String),
    EditCancel,
    SetColor(Uuid, CueColor),
    AssignTarget(Uuid, Uuid),
}

fn status_label(state: &CueState) -> (&'static str, Color32) {
    match state {
        CueState::Stopped => ("stopped", Theme::TEXT_DIM),
        CueState::Waiting => ("⏳ pre-wait", Theme::PAUSE_BTN),
        CueState::Playing => ("▶ playing", Theme::TEXT_ACTIVE),
        CueState::Paused => ("⏸ paused", Theme::PAUSE_BTN),
        CueState::FadingIn => ("↗ fade in", Theme::ACCENT2),
        CueState::FadingOut => ("↘ fade out", Theme::ACCENT),
        CueState::Loading => ("loading…", Theme::ACCENT),
        CueState::Error(_) => ("⚠ error", Theme::STOP_BTN),
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
