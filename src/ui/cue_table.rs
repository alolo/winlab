// Cue table — main QLab-style cue list view

use crate::cue::{AudioCue, CueColor, CueState};
use crate::ui::theme::Theme;
use egui::*;
use uuid::Uuid;

pub struct CueTable;

impl CueTable {
    /// Draw the cue list table. Returns (selected_id, action_triggered_id)
    pub fn show(
        ui: &mut Ui,
        cues: &mut Vec<AudioCue>,
        selected: &mut Option<Uuid>,
        renaming: &mut Option<(Uuid, String)>,
    ) -> Option<CueTableAction> {
        let mut action = None;

        // Column headers
        let row_h = 28.0;
        let col_widths = [50.0, 60.0, 280.0, 120.0, 80.0, 100.0, 80.0];

        // Header
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            let header_color = Theme::TEXT_DIM;
            for (label, w) in [
                ("#", col_widths[0]),
                ("Cue", col_widths[1]),
                ("Name", col_widths[2]),
                ("File", col_widths[3]),
                ("Duration", col_widths[4]),
                ("Status", col_widths[5]),
                ("Hotkey", col_widths[6]),
            ] {
                ui.add_sized(
                    [w, 20.0],
                    egui::Label::new(RichText::new(label).color(header_color).size(11.0)),
                );
            }
        });

        ui.add(Separator::default().horizontal());

        // Rows
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (idx, cue) in cues.iter_mut().enumerate() {
                    let is_selected = *selected == Some(cue.id);
                    let row_color = Self::row_bg(cue, is_selected, idx);

                    let resp = ui.horizontal(|ui| {
                        // Color strip on left
                        let (strip_rect, _) = ui.allocate_exact_size(
                            vec2(4.0, row_h),
                            Sense::hover(),
                        );
                        ui.painter().rect_filled(strip_rect, 0.0, cue.color.to_egui());
                        ui.add_space(2.0);

                        // Row background
                        let full_rect = ui.available_rect_before_wrap();
                        let row_rect = Rect::from_min_size(
                            ui.cursor().min - vec2(6.0, 0.0),
                            vec2(ui.available_width() + 6.0, row_h),
                        );

                        // Index
                        ui.add_sized(
                            [col_widths[0], row_h],
                            Label::new(
                                RichText::new(format!("{}", idx + 1))
                                    .color(Theme::TEXT_DIM)
                                    .size(11.0),
                            ),
                        );

                        // Cue number (editable)
                        ui.add_sized(
                            [col_widths[1], row_h],
                            Label::new(
                                RichText::new(&cue.number)
                                    .color(Theme::ACCENT)
                                    .monospace()
                                    .size(12.0),
                            ),
                        );

                        // Name (double-click to rename)
                        let is_renaming = renaming
                            .as_ref()
                            .map(|(id, _)| *id == cue.id)
                            .unwrap_or(false);

                        if is_renaming {
                            if let Some((_, ref mut buf)) = renaming {
                                let resp = ui.add_sized(
                                    [col_widths[2], row_h],
                                    TextEdit::singleline(buf).font(TextStyle::Body),
                                );
                                resp.request_focus();
                                if resp.lost_focus() || ui.input(|i| i.key_pressed(Key::Enter)) {
                                    action = Some(CueTableAction::RenameConfirm(
                                        cue.id,
                                        buf.clone(),
                                    ));
                                }
                                if ui.input(|i| i.key_pressed(Key::Escape)) {
                                    action = Some(CueTableAction::RenameCancel);
                                }
                            }
                        } else {
                            let name_resp = ui.add_sized(
                                [col_widths[2], row_h],
                                Label::new(
                                    RichText::new(&cue.name)
                                        .color(Self::name_color(cue))
                                        .size(13.0),
                                )
                                .sense(Sense::click()),
                            );
                            if name_resp.double_clicked() {
                                action = Some(CueTableAction::StartRename(cue.id, cue.name.clone()));
                            }
                        }

                        // File name
                        let fname = cue
                            .file_path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("—");
                        ui.add_sized(
                            [col_widths[3], row_h],
                            Label::new(
                                RichText::new(fname)
                                    .color(Theme::TEXT_DIM)
                                    .size(11.0),
                            ),
                        );

                        // Duration
                        let dur = format_duration(cue.duration_secs);
                        ui.add_sized(
                            [col_widths[4], row_h],
                            Label::new(
                                RichText::new(dur)
                                    .color(Theme::TEXT_DIM)
                                    .monospace()
                                    .size(11.0),
                            ),
                        );

                        // Status with progress
                        ui.add_sized(
                            [col_widths[5], row_h],
                            |ui: &mut Ui| {
                                let (rect, resp) = ui.allocate_exact_size(
                                    vec2(col_widths[5], row_h - 8.0),
                                    Sense::hover(),
                                );
                                if cue.is_active() && cue.duration_secs > 0.0 {
                                    // Progress bar
                                    let progress = cue.progress();
                                    let bar_rect = Rect::from_min_size(
                                        rect.min,
                                        vec2(rect.width() * progress, rect.height()),
                                    );
                                    ui.painter().rect_filled(bar_rect, 2.0, Theme::BG_ACTIVE.linear_multiply(0.6));
                                    // Remaining time text
                                    let remaining = format_duration(cue.remaining_secs());
                                    ui.painter().text(
                                        rect.center(),
                                        Align2::CENTER_CENTER,
                                        remaining,
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
                            },
                        );

                        // Hotkey
                        let hk = cue
                            .hotkey
                            .as_ref()
                            .map(|h| h.key.as_str())
                            .unwrap_or("—");
                        ui.add_sized(
                            [col_widths[6], row_h],
                            Label::new(
                                RichText::new(hk)
                                    .color(Theme::TEXT_DIM)
                                    .monospace()
                                    .size(11.0),
                            ),
                        );
                    });

                    // Full-row click handling
                    let row_resp = ui.interact(
                        resp.response.rect,
                        Id::new(("cue_row", cue.id)),
                        Sense::click(),
                    );

                    // Background fill
                    if is_selected || cue.is_active() || cue.state == CueState::Paused {
                        ui.painter().rect_filled(row_resp.rect, 0.0, row_color.linear_multiply(0.5));
                    }

                    if row_resp.clicked() {
                        *selected = Some(cue.id);
                    }
                    if row_resp.double_clicked() && !matches!(action, Some(CueTableAction::StartRename(..))) {
                        action = Some(CueTableAction::Play(cue.id));
                    }

                    // Context menu
                    row_resp.context_menu(|ui| {
                        if ui.button("▶  Play").clicked() {
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
                        if ui.button("✏  Rename").clicked() {
                            action = Some(CueTableAction::StartRename(cue.id, cue.name.clone()));
                            ui.close_menu();
                        }
                        if ui.button("🗑  Delete").clicked() {
                            action = Some(CueTableAction::Delete(cue.id));
                            ui.close_menu();
                        }
                        ui.separator();
                        ui.menu_button("🎨  Color", |ui| {
                            for (label, color) in [
                                ("None",   CueColor::NONE),
                                ("Red",    CueColor::RED),
                                ("Green",  CueColor::GREEN),
                                ("Blue",   CueColor::BLUE),
                                ("Yellow", CueColor::YELLOW),
                                ("Purple", CueColor::PURPLE),
                                ("Orange", CueColor::ORANGE),
                            ] {
                                if ui
                                    .add(egui::Button::new(
                                        RichText::new(label).color(color.to_egui()),
                                    ))
                                    .clicked()
                                {
                                    action = Some(CueTableAction::SetColor(cue.id, color));
                                    ui.close_menu();
                                }
                            }
                        });
                    });

                    ui.add_space(1.0);
                }
            });

        action
    }

    fn row_bg(cue: &AudioCue, selected: bool, idx: usize) -> Color32 {
        if selected {
            return Theme::BG_SELECTED;
        }
        match cue.state {
            CueState::Playing | CueState::FadingIn | CueState::FadingOut => Theme::BG_ACTIVE,
            CueState::Paused => Theme::BG_PAUSED,
            _ => {
                if idx % 2 == 0 {
                    Theme::BG_ROW
                } else {
                    Theme::BG_ROW_ALT
                }
            }
        }
    }

    fn name_color(cue: &AudioCue) -> Color32 {
        match cue.state {
            CueState::Playing | CueState::FadingIn => Theme::TEXT_ACTIVE,
            CueState::Paused => Theme::PAUSE_BTN,
            _ => Theme::TEXT,
        }
    }
}

pub enum CueTableAction {
    Play(Uuid),
    Stop(Uuid),
    Pause(Uuid),
    Delete(Uuid),
    StartRename(Uuid, String),
    RenameConfirm(Uuid, String),
    RenameCancel,
    SetColor(Uuid, CueColor),
}

fn status_label(state: &CueState) -> (&'static str, Color32) {
    match state {
        CueState::Stopped => ("stopped", Theme::TEXT_DIM),
        CueState::Playing => ("▶ playing", Theme::TEXT_ACTIVE),
        CueState::Paused  => ("⏸ paused",  Theme::PAUSE_BTN),
        CueState::FadingIn  => ("▶ fade in",  Theme::ACCENT2),
        CueState::FadingOut => ("▶ fade out", Theme::ACCENT),
        CueState::Waiting => ("◉ ready", Theme::ACCENT),
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
