// Inspector panel — properties of selected cue

use crate::cue::{AudioCue, FadeCurve, LoopRegion};
use crate::ui::theme::Theme;
use egui::*;

pub struct Inspector;

impl Inspector {
    pub fn show(ui: &mut Ui, cue: &mut AudioCue) {
        ui.add_space(4.0);

        // Header
        ui.horizontal(|ui| {
            ui.label(RichText::new("⚙ Inspector").color(Theme::ACCENT).size(13.0).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(&cue.name).color(Theme::TEXT_DIM).size(11.0).italics(),
                );
            });
        });

        ui.add(Separator::default().horizontal());
        ui.add_space(4.0);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Basic info
                Self::section(ui, "Identity", |ui| {
                    Self::field(ui, "Number", |ui| {
                        ui.add(TextEdit::singleline(&mut cue.number).desired_width(80.0));
                    });
                    Self::field(ui, "Name", |ui| {
                        ui.add(TextEdit::singleline(&mut cue.name).desired_width(200.0));
                    });
                    Self::field(ui, "Notes", |ui| {
                        ui.add(
                            TextEdit::multiline(&mut cue.notes)
                                .desired_width(200.0)
                                .desired_rows(2),
                        );
                    });
                });

                ui.add_space(6.0);

                // File
                Self::section(ui, "File", |ui| {
                    let fname = cue
                        .file_path
                        .to_string_lossy()
                        .to_string();
                    ui.label(RichText::new(&fname).color(Theme::TEXT_DIM).size(10.0));
                    ui.label(
                        RichText::new(crate::ui::cue_table::format_duration(cue.duration_secs))
                            .color(Theme::ACCENT)
                            .monospace()
                            .size(12.0),
                    );
                });

                ui.add_space(6.0);

                // Playback
                Self::section(ui, "Playback", |ui| {
                    Self::field(ui, "Volume", |ui| {
                        let db = vol_to_db(cue.volume);
                        let mut db_val = db;
                        ui.add(
                            DragValue::new(&mut db_val)
                                .range(-60.0..=6.0)
                                .speed(0.1)
                                .suffix(" dB"),
                        );
                        cue.volume = db_to_vol(db_val);
                    });
                    Self::field(ui, "Fade In", |ui| {
                        ui.add(
                            DragValue::new(&mut cue.fade_in_secs)
                                .range(0.0..=30.0)
                                .speed(0.05)
                                .suffix(" s"),
                        );
                    });
                    Self::field(ui, "Fade Out", |ui| {
                        ui.add(
                            DragValue::new(&mut cue.fade_out_secs)
                                .range(0.0..=30.0)
                                .speed(0.05)
                                .suffix(" s"),
                        );
                    });
                    Self::field(ui, "Pre-delay", |ui| {
                        ui.add(
                            DragValue::new(&mut cue.pre_delay_secs)
                                .range(0.0..=60.0)
                                .speed(0.05)
                                .suffix(" s"),
                        );
                    });
                    Self::field(ui, "Fade curve", |ui| {
                        egui::ComboBox::from_id_salt("fade_curve")
                            .selected_text(format!("{:?}", cue.fade_curve))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut cue.fade_curve, FadeCurve::Linear, "Linear");
                                ui.selectable_value(&mut cue.fade_curve, FadeCurve::Logarithmic, "Logarithmic");
                                ui.selectable_value(&mut cue.fade_curve, FadeCurve::SCurve, "S-Curve");
                            });
                    });
                });

                ui.add_space(6.0);

                // Region
                Self::section(ui, "Region & Loop", |ui| {
                    Self::field(ui, "Start", |ui| {
                        ui.add(
                            DragValue::new(&mut cue.start_secs)
                                .range(0.0..=cue.duration_secs.max(1.0))
                                .speed(0.01)
                                .suffix(" s"),
                        );
                    });
                    if let Some(ref mut end) = cue.end_secs {
                        Self::field(ui, "End", |ui| {
                            ui.add(
                                DragValue::new(end)
                                    .range(cue.start_secs..=cue.duration_secs.max(1.0))
                                    .speed(0.01)
                                    .suffix(" s"),
                            );
                        });
                    }
                    let mut has_end = cue.end_secs.is_some();
                    if ui.checkbox(&mut has_end, "Custom end point").changed() {
                        cue.end_secs = if has_end { Some(cue.duration_secs) } else { None };
                    }

                    ui.add_space(4.0);
                    ui.checkbox(&mut cue.loop_region.enabled, "Enable loop");
                    if cue.loop_region.enabled {
                        Self::field(ui, "Loop start", |ui| {
                            ui.add(
                                DragValue::new(&mut cue.loop_region.start_secs)
                                    .range(0.0..=cue.duration_secs.max(1.0))
                                    .speed(0.01)
                                    .suffix(" s"),
                            );
                        });
                        Self::field(ui, "Loop end", |ui| {
                            ui.add(
                                DragValue::new(&mut cue.loop_region.end_secs)
                                    .range(cue.loop_region.start_secs..=cue.duration_secs.max(1.0))
                                    .speed(0.01)
                                    .suffix(" s"),
                            );
                        });
                        Self::field(ui, "Repeat", |ui| {
                            let mut inf = cue.loop_region.count.is_none();
                            ui.checkbox(&mut inf, "∞");
                            if inf {
                                cue.loop_region.count = None;
                            } else {
                                let mut n = cue.loop_region.count.unwrap_or(2);
                                ui.add(DragValue::new(&mut n).range(1..=999).suffix("×"));
                                cue.loop_region.count = Some(n);
                            }
                        });
                    }
                });

                ui.add_space(6.0);

                // Routing
                Self::section(ui, "Output Routing", |ui| {
                    Self::field(ui, "L channel", |ui| {
                        ui.add(DragValue::new(&mut cue.channel_route.left).range(0..=63));
                    });
                    Self::field(ui, "R channel", |ui| {
                        ui.add(DragValue::new(&mut cue.channel_route.right).range(0..=63));
                    });
                });

                ui.add_space(6.0);

                // Hotkey display
                Self::section(ui, "Trigger", |ui| {
                    let hk = cue
                        .hotkey
                        .as_ref()
                        .map(|h| h.key.clone())
                        .unwrap_or_else(|| "None".to_string());
                    ui.label(RichText::new(format!("Hotkey: {hk}")).color(Theme::TEXT_DIM).size(11.0));

                    let midi = cue
                        .midi_trigger
                        .as_ref()
                        .map(|m| format!("{m:?}"))
                        .unwrap_or_else(|| "None".to_string());
                    ui.label(RichText::new(format!("MIDI: {midi}")).color(Theme::TEXT_DIM).size(11.0));
                });
            });
    }

    fn section(ui: &mut Ui, title: &str, content: impl FnOnce(&mut Ui)) {
        ui.label(RichText::new(title).color(Theme::ACCENT).size(11.0).strong());
        ui.indent(title, |ui| {
            content(ui);
        });
    }

    fn field(ui: &mut Ui, label: &str, content: impl FnOnce(&mut Ui)) {
        ui.horizontal(|ui| {
            ui.add_sized(
                [70.0, 20.0],
                Label::new(RichText::new(label).color(Theme::TEXT_DIM).size(11.0)),
            );
            content(ui);
        });
    }
}

fn vol_to_db(vol: f32) -> f32 {
    if vol <= 0.0 { -60.0 } else { 20.0 * vol.log10() }
}

fn db_to_vol(db: f32) -> f32 {
    if db <= -60.0 { 0.0 } else { 10f32.powf(db / 20.0) }
}
