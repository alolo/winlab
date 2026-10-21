// Transport bar — GO, STOP, PAUSE buttons and master volume

use crate::ui::theme::Theme;
use egui::*;

pub enum TransportAction {
    Go,
    StopSelected,
    StopAll,
    PauseSelected,
    PauseAll,
    ResumeSelected,
    ResumeAll,
}

pub struct TransportBar;

impl TransportBar {
    pub fn show(ui: &mut Ui, active_count: usize, master_vol: &mut f32) -> Option<TransportAction> {
        let mut action = None;

        ui.horizontal(|ui| {
            ui.add_space(4.0);

            // GO button — big green
            let go_resp = ui.add_sized(
                [80.0, 44.0],
                Button::new(
                    RichText::new("▶  GO")
                        .color(Color32::WHITE)
                        .size(15.0)
                        .strong(),
                )
                .fill(Theme::GO_BTN)
                .rounding(6.0),
            );
            if go_resp.clicked() {
                action = Some(TransportAction::Go);
            }
            go_resp.on_hover_text("Play selected cue (Space)");

            ui.add_space(8.0);

            // STOP selected
            if ui
                .add_sized(
                    [60.0, 44.0],
                    Button::new(RichText::new("⏹").size(18.0))
                        .fill(Theme::STOP_BTN)
                        .rounding(6.0),
                )
                .on_hover_text("Stop selected (S)")
                .clicked()
            {
                action = Some(TransportAction::StopSelected);
            }

            // PAUSE selected
            if ui
                .add_sized(
                    [60.0, 44.0],
                    Button::new(RichText::new("⏸").size(18.0))
                        .fill(Theme::PAUSE_BTN)
                        .rounding(6.0),
                )
                .on_hover_text("Pause selected (P)")
                .clicked()
            {
                action = Some(TransportAction::PauseSelected);
            }

            // RESUME selected
            if ui
                .add_sized(
                    [60.0, 44.0],
                    Button::new(RichText::new("▶").size(18.0))
                        .fill(Color32::from_rgb(40, 100, 60))
                        .rounding(6.0),
                )
                .on_hover_text("Resume selected (R)")
                .clicked()
            {
                action = Some(TransportAction::ResumeSelected);
            }

            ui.add_space(12.0);
            ui.add(Separator::default().vertical());
            ui.add_space(8.0);

            // ALL buttons
            ui.vertical(|ui| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            Button::new(
                                RichText::new("ALL STOP").color(Theme::STOP_BTN).size(11.0),
                            )
                            .fill(Color32::from_rgb(50, 20, 20))
                            .rounding(4.0),
                        )
                        .on_hover_text("Stop all cues (Escape)")
                        .clicked()
                    {
                        action = Some(TransportAction::StopAll);
                    }

                    if ui
                        .add(
                            Button::new(
                                RichText::new("ALL PAUSE").color(Theme::PAUSE_BTN).size(11.0),
                            )
                            .fill(Color32::from_rgb(50, 40, 10))
                            .rounding(4.0),
                        )
                        .on_hover_text("Pause all (Shift+P)")
                        .clicked()
                    {
                        action = Some(TransportAction::PauseAll);
                    }

                    if ui
                        .add(
                            Button::new(
                                RichText::new("ALL RESUME").color(Theme::ACCENT2).size(11.0),
                            )
                            .fill(Color32::from_rgb(15, 50, 30))
                            .rounding(4.0),
                        )
                        .on_hover_text("Resume all (Shift+R)")
                        .clicked()
                    {
                        action = Some(TransportAction::ResumeAll);
                    }
                });
            });

            ui.add_space(12.0);
            ui.add(Separator::default().vertical());
            ui.add_space(8.0);

            // Active cue indicator
            ui.vertical(|ui| {
                ui.add_space(4.0);
                let color = if active_count > 0 { Theme::TEXT_ACTIVE } else { Theme::TEXT_DIM };
                ui.label(
                    RichText::new(format!("● {active_count} active"))
                        .color(color)
                        .size(12.0),
                );
            });

            // Master volume
            ui.add_space(12.0);
            ui.add(Separator::default().vertical());
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.add_space(2.0);
                ui.label(RichText::new("MASTER").color(Theme::TEXT_DIM).size(10.0));
                ui.add(
                    Slider::new(master_vol, 0.0..=1.0)
                        .show_value(true)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                        .text("")
                );
            });
        });

        action
    }
}
