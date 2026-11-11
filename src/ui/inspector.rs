// src/ui/inspector.rs — вкладки нижней панели: Inspector, Playback, I/O Routing и MIDI

use crate::audio::engine::AudioEngine;
use crate::config::AudioBackend;
use crate::cue::{
    ContinueMode, Cue, CueColor, CueType, FadeCurve, GroupMode, MidiMessageKind, OscCueArg,
};
use crate::midi::input::MidiOutputManager;
use crate::ui::theme::Theme;
use egui::*;
use rfd::FileDialog;
use std::path::PathBuf;
use uuid::Uuid;

pub struct Inspector;

#[derive(Debug, Clone)]
pub enum InspectorAction {
    SetAudioFile(PathBuf),
}

impl Inspector {
    /// Вкладка 1: Основные свойства, тайминги Pre/Post-Wait, выбор цели (для Play/Pause/Stop/Fade) и состав группы
    pub fn show_inspector(
        ui: &mut Ui,
        cue: &mut Cue,
        cue_targets: &[(Uuid, String, Option<std::path::PathBuf>)],
        all_cues: &[(Uuid, String)],
    ) -> Option<InspectorAction> {
        let mut action = None;
        ui.columns(3, |cols| {
                    // Колонка 1: Identity & Color
                    Self::section(&mut cols[0], "Identity", |ui| {
                        Self::field(ui, "Number", |ui| {
                            ui.add(TextEdit::singleline(&mut cue.number).desired_width(70.0));
                        });
                        Self::field(ui, "Name", |ui| {
                            ui.add(TextEdit::singleline(&mut cue.name).desired_width(180.0));
                        });
                        Self::field(ui, "Color", |ui| {
                            egui::ComboBox::from_id_salt("inspector_color")
                                .selected_text(cue.color.label())
                                .show_ui(ui, |ui| {
                                    for c in CueColor::ALL {
                                        ui.selectable_value(&mut cue.color, *c, c.label());
                                    }
                                });
                        });
                        Self::field(ui, "Notes", |ui| {
                            ui.add(
                                TextEdit::multiline(&mut cue.notes)
                                    .desired_width(180.0)
                                    .desired_rows(2),
                            );
                        });
                    });

                    // Колонка 2: Timing (Pre-Wait / Post-Wait / Continue Mode на всех событиях)
                    Self::section(&mut cols[1], "Timing & Continue", |ui| {
                        Self::field(ui, "Pre-Wait", |ui| {
                            ui.add(
                                DragValue::new(&mut cue.pre_wait_secs)
                                    .range(0.0..=3600.0)
                                    .speed(0.1)
                                    .suffix(" s"),
                            );
                        });
                        Self::field(ui, "Post-Wait", |ui| {
                            ui.add(
                                DragValue::new(&mut cue.post_wait_secs)
                                    .range(0.0..=3600.0)
                                    .speed(0.1)
                                    .suffix(" s"),
                            );
                        });
                        Self::field(ui, "Continue", |ui| {
                            egui::ComboBox::from_id_salt("inspector_continue")
                                .selected_text(cue.continue_mode.label())
                                .show_ui(ui, |ui| {
                                    for m in ContinueMode::ALL {
                                        ui.selectable_value(
                                            &mut cue.continue_mode,
                                            m.clone(),
                                            m.label(),
                                        );
                                    }
                                });
                        });
                        Self::field(ui, "Hotkey", |ui| {
                            let mut hk = cue.hotkey.clone().unwrap_or_default();
                            if ui
                                .add(
                                    TextEdit::singleline(&mut hk)
                                        .hint_text("e.g. F1, 1, Q")
                                        .desired_width(100.0),
                                )
                                .changed()
                            {
                                cue.hotkey = if hk.trim().is_empty() {
                                    None
                                } else {
                                    Some(hk.trim().to_uppercase())
                                };
                            }
                        });
                    });

                    // Колонка 3: Специфичные настройки типа события (Target / Group / File)
                    Self::section(&mut cols[2], "Event Target / Content", |ui| {
                        let cue_id = cue.id;
                        match &mut cue.cue_type {
                            CueType::Audio => {
                                let file_exists = cue.file_path.is_file();
                                let fname = if cue.file_path.as_os_str().is_empty() {
                                    "No audio file assigned".to_string()
                                } else {
                                    cue.file_path.to_string_lossy().to_string()
                                };
                                ui.label(
                                    RichText::new(fname)
                                        .color(if file_exists {
                                            Theme::text_dim()
                                        } else {
                                            egui::Color32::from_rgb(235, 85, 85)
                                        })
                                        .size(11.0),
                                )
                                .on_hover_text(if file_exists {
                                    "Audio file is available"
                                } else {
                                    "Audio file is missing. Locate it or replace it."
                                });
                                ui.horizontal(|ui| {
                                    let button_label = if file_exists {
                                        "Replace File…"
                                    } else {
                                        "Locate File…"
                                    };
                                    if ui.button(button_label).clicked() {
                                        if let Some(path) = FileDialog::new()
                                            .add_filter(
                                                "Audio",
                                                &["wav", "mp3", "flac", "ogg", "aiff", "aif", "m4a"],
                                            )
                                            .pick_file()
                                        {
                                            action = Some(InspectorAction::SetAudioFile(path));
                                        }
                                    }
                                    if !file_exists {
                                        ui.colored_label(
                                            egui::Color32::from_rgb(235, 85, 85),
                                            "Missing media",
                                        );
                                    }
                                });
                                ui.label(
                                    RichText::new(format!(
                                        "Duration: {}",
                                        crate::ui::cue_table::format_duration(cue.duration_secs)
                                    ))
                                    .color(Theme::accent())
                                    .monospace(),
                                );
                            }

                            CueType::Play { target_id }
                            | CueType::Pause { target_id }
                            | CueType::Stop { target_id } => {
                                Self::target_selector(ui, target_id, &mut cue.file_path, cue_targets);
                            }

                            CueType::Fade {
                                target_id,
                                fade_in,
                                duration_secs,
                                stop_on_finish,
                                target_volume,
                                fade_curve,
                                custom_fade_points,
                            } => {
                                Self::target_selector(ui, target_id, &mut cue.file_path, cue_targets);
                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    ui.selectable_value(fade_in, true, "↗ Fade In");
                                    ui.selectable_value(fade_in, false, "↘ Fade Out");
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Duration:");
                                    ui.add(
                                        DragValue::new(duration_secs)
                                            .range(0.05..=600.0)
                                            .speed(0.1)
                                            .fixed_decimals(1)
                                            .suffix(" s"),
                                    );
                                });

                                let mut clear_target_level = false;
                                let mut set_target_level = false;
                                ui.horizontal(|ui| {
                                    ui.label("Target level:");
                                    if target_volume.is_some() {
                                        if let Some(level) = target_volume.as_mut() {
                                            let mut db = vol_to_db(*level);
                                            if ui.add(DragValue::new(&mut db)
                                                .range(-60.0..=6.0)
                                                .speed(0.1)
                                                .suffix(" dB")).changed() {
                                                *level = db_to_vol(db);
                                            }
                                        }
                                        clear_target_level = ui.small_button("Target default").clicked();
                                    } else {
                                        let default_label = if *fade_in {
                                            "Follow target cue volume"
                                        } else {
                                            "Silence (0.0 linear)"
                                        };
                                        ui.label(RichText::new(if *fade_in {
                                        "Follow target volume (0 dB if muted)"
                                    } else {
                                        default_label
                                    }).color(Theme::text_dim()).size(10.5));
                                        set_target_level = ui.small_button("Set level").clicked();
                                    }
                                });
                                if clear_target_level {
                                    *target_volume = None;
                                } else if set_target_level {
                                    *target_volume = Some(if *fade_in { 1.0 } else { 0.0 });
                                }

                                ui.horizontal(|ui| {
                                    ui.label("Fade curve:");
                                    egui::ComboBox::from_id_salt(("cue_fade_curve", cue_id))
                                        .selected_text(match fade_curve {
                                            FadeCurve::Linear => "Linear",
                                            FadeCurve::SCurve => "S-Curve",
                                            FadeCurve::Logarithmic => "Logarithmic",
                                            FadeCurve::Exponential => "Exponential",
                                            FadeCurve::Custom => "Custom",
                                        })
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(fade_curve, FadeCurve::Linear, "Linear");
                                            ui.selectable_value(fade_curve, FadeCurve::SCurve, "S-Curve");
                                            ui.selectable_value(fade_curve, FadeCurve::Logarithmic, "Logarithmic");
                                            ui.selectable_value(fade_curve, FadeCurve::Exponential, "Exponential");
                                            ui.selectable_value(fade_curve, FadeCurve::Custom, "Custom");
                                        });
                                });
                                if *fade_curve == FadeCurve::Custom {
                                    if custom_fade_points.len() < 2 {
                                        *custom_fade_points = vec![[0.0, 0.0], [1.0, 1.0]];
                                    }
                                    Self::custom_fade_editor(ui, custom_fade_points, *fade_in);
                                } else {
                                    Self::fade_curve_preview(ui, *fade_curve, *fade_in);
                                }
                                if !*fade_in {
                                    ui.checkbox(stop_on_finish, "Stop target when fade finishes");
                                }
                            }

CueType::Group { mode, children, fade_in_secs, fade_out_secs } => {
    ui.horizontal(|ui| {
        ui.label("Mode:");
        egui::ComboBox::from_id_salt("group_mode")
            .selected_text(mode.label())
            .show_ui(ui, |ui| {
                for m in GroupMode::ALL {
                    ui.selectable_value(mode, m.clone(), m.label());
                }
            });
    });

    match mode {
        GroupMode::Normal => {
            ui.label(RichText::new(
                "Normal: triggers only the first child, then follows \
                 auto-continue/auto-follow chain of children."
            ).color(Theme::text_dim()).size(11.0));
        }
        GroupMode::Playlist => {
            ui.label(RichText::new(
                "Playlist: plays children sequentially. Each child waits for its pre-wait, playback completion, and post-wait."
            ).color(Theme::text_dim()).size(11.0));

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Group Fade In:");
                ui.add(DragValue::new(fade_in_secs).range(0.0..=60.0)
                    .speed(0.05).suffix(" s"));
            });
            ui.horizontal(|ui| {
                ui.label("Group Fade Out:");
                ui.add(DragValue::new(fade_out_secs).range(0.0..=60.0)
                    .speed(0.05).suffix(" s"));
            });
        }
        GroupMode::AllCues => {
            ui.label(RichText::new(
                "All Cues: starts every child simultaneously. Each child observes its own pre-wait; the group completes after all children and their post-waits."
            ).color(Theme::text_dim()).size(11.0));
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Group Fade In:");
                ui.add(DragValue::new(fade_in_secs).range(0.0..=60.0)
                    .speed(0.05).suffix(" s"));
            });
            ui.horizontal(|ui| {
                ui.label("Group Fade Out:");
                ui.add(DragValue::new(fade_out_secs).range(0.0..=60.0)
                    .speed(0.05).suffix(" s"));
            });
        }
    }

    ui.add_space(6.0);
    ui.label(RichText::new(format!(
        "Children: {} (drag cues onto the group row in the table to add, or use context menu)",
        children.len()
    )).color(Theme::accent()).size(11.0));

    // Список детей только для справки (без чекбоксов)
    egui::ScrollArea::vertical()
        .id_salt(("group_children_scroll", cue_id))
        .max_height(90.0)
        .show(ui, |ui| {
        for (cid, clabel) in all_cues {
            if *cid == cue_id { continue; }
            if children.contains(cid) {
                ui.label(RichText::new(format!("  • {}", clabel))
                    .color(Theme::text_dim()).size(11.0));
            }
        }
    });
}

                            CueType::Midi { .. } => {
                                ui.label(
                                    RichText::new("Switch to the '🎹 MIDI' tab to configure MIDI parameters and raw hex code.")
                                        .color(Theme::accent())
                                        .size(11.5),
                                );
                            }

                            CueType::Comment { text } => {
                                ui.label(
                                    RichText::new("Comment cues are passive markers. GO skips them without triggering an event.")
                                        .color(Theme::text_dim())
                                        .size(11.0),
                                );
                                ui.add_space(6.0);
                                ui.label(RichText::new("Comment text").color(Theme::accent()).strong());
                                let comment_width = ui.available_width();
                                ui.add(
                                    TextEdit::multiline(text)
                                        .desired_width(comment_width)
                                        .desired_rows(6)
                                        .hint_text("Write a stage direction, reminder or show note…"),
                                );
                            }

                            CueType::Osc { address, args } => {
                                Self::field(ui, "Address", |ui| {
                                    ui.add(
                                        TextEdit::singleline(address)
                                            .hint_text("/show/cue/go")
                                            .desired_width(220.0),
                                    );
                                });
                                ui.add_space(6.0);
                                ui.label(RichText::new("Arguments").color(Theme::accent()).strong());
                                let mut remove_index = None;
                                for (index, arg) in args.iter_mut().enumerate() {
                                    ui.push_id(("osc_arg", cue_id, index), |ui| {
                                        ui.horizontal(|ui| {
                                            egui::ComboBox::from_id_salt("osc_arg_type")
                                                .width(86.0)
                                                .selected_text(arg.label())
                                                .show_ui(ui, |ui| {
                                                    if ui.selectable_label(matches!(arg, OscCueArg::Int(_)), "Integer").clicked() {
                                                        *arg = OscCueArg::Int(0);
                                                    }
                                                    if ui.selectable_label(matches!(arg, OscCueArg::Float(_)), "Float").clicked() {
                                                        *arg = OscCueArg::Float(0.0);
                                                    }
                                                    if ui.selectable_label(matches!(arg, OscCueArg::String(_)), "String").clicked() {
                                                        *arg = OscCueArg::String(String::new());
                                                    }
                                                    if ui.selectable_label(matches!(arg, OscCueArg::Bool(_)), "Boolean").clicked() {
                                                        *arg = OscCueArg::Bool(true);
                                                    }
                                                });
                                            match arg {
                                                OscCueArg::Int(value) => { ui.add(DragValue::new(value)); }
                                                OscCueArg::Float(value) => { ui.add(DragValue::new(value).speed(0.05)); }
                                                OscCueArg::String(value) => {
                                                    ui.add(TextEdit::singleline(value).desired_width(110.0));
                                                }
                                                OscCueArg::Bool(value) => { ui.checkbox(value, "True"); }
                                            }
                                            if ui.small_button("×").clicked() {
                                                remove_index = Some(index);
                                            }
                                        });
                                    });
                                }
                                if let Some(index) = remove_index {
                                    args.remove(index);
                                }
                                ui.horizontal(|ui| {
                                    if ui.button("+ Integer").clicked() { args.push(OscCueArg::Int(0)); }
                                    if ui.button("+ Float").clicked() { args.push(OscCueArg::Float(0.0)); }
                                    if ui.button("+ String").clicked() { args.push(OscCueArg::String(String::new())); }
                                    if ui.button("+ Boolean").clicked() { args.push(OscCueArg::Bool(true)); }
                                });
                                ui.label(
                                    RichText::new("The event sends this OSC message through the configured global send address.")
                                        .color(Theme::text_dim())
                                        .size(10.5),
                                );
                            }
                        }

                        ui.add_space(6.0);
                        ui.separator();
                        ui.label(
                            RichText::new("Time Triggers")
                                .strong()
                                .color(Theme::accent()),
                        );
                        let is_comment = matches!(&cue.cue_type, CueType::Comment { .. });
                        ui.add_enabled_ui(!is_comment, |ui| {
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut cue.time_trigger_enabled, "Daily");
                                ui.label("Local time:");
                                ui.add(
                                    TextEdit::singleline(&mut cue.time_trigger_time)
                                        .desired_width(86.0)
                                        .hint_text("HH:MM"),
                                );
                            });
                        });
                        if is_comment {
                            ui.label(
                                RichText::new("Comment cues cannot be started by a time trigger.")
                                    .color(Theme::text_dim())
                                    .size(10.5),
                            );
                        } else if chrono::NaiveTime::parse_from_str(
                            &cue.time_trigger_time,
                            "%H:%M",
                        )
                        .is_err()
                            && chrono::NaiveTime::parse_from_str(
                                &cue.time_trigger_time,
                                "%H:%M:%S",
                            )
                            .is_err()
                        {
                            ui.colored_label(
                                egui::Color32::from_rgb(235, 85, 85),
                                "Use 24-hour HH:MM (or HH:MM:SS), e.g. 19:30.",
                            );
                        } else {
                            ui.label(
                                RichText::new(if cue.time_trigger_enabled {
                                    "Runs once per day using the computer's local clock."
                                } else {
                                    "Trigger is disabled."
                                })
                                .color(Theme::text_dim())
                                .size(10.5),
                            );
                        }
                    });
                });

        action
    }

    fn target_selector(
        ui: &mut Ui,
        target_id: &mut Option<Uuid>,
        cue_file_path: &mut std::path::PathBuf,
        cue_targets: &[(Uuid, String, Option<std::path::PathBuf>)],
    ) {
        let selected_label = target_id
            .and_then(|id| cue_targets.iter().find(|(tid, _, _)| *tid == id))
            .map(|(_, label, _)| label.as_str())
            .unwrap_or("None / All Active");

        ui.label(
            RichText::new("Target Cue / Group:")
                .color(Theme::text_dim())
                .size(11.0),
        );
        egui::ComboBox::from_id_salt("target_cue_or_group")
            .width(240.0)
            .selected_text(selected_label)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(target_id.is_none(), "None / All Active")
                    .clicked()
                {
                    *target_id = None;
                }
                for (tid, label, file_path) in cue_targets {
                    if ui
                        .selectable_label(*target_id == Some(*tid), label)
                        .clicked()
                    {
                        *target_id = Some(*tid);
                        if let Some(path) = file_path {
                            *cue_file_path = path.clone();
                        }
                    }
                }
            });
    }

    /// Вкладка 2: Playback
    pub fn show_playback(ui: &mut Ui, cue: &mut Cue) -> (bool, bool) {
        let mut volume_changed = false;
        let mut time_pitch_changed = false;

        // Keep the two related control groups side by side, matching the
        // multi-column layout used by the main Inspector.
        ui.columns(2, |cols| {
            Self::section(&mut cols[0], "Levels & Envelope", |ui| {
                Self::field(ui, "Volume", |ui| {
                    let mut db_val = vol_to_db(cue.volume);
                    if ui
                        .add(
                            DragValue::new(&mut db_val)
                                .range(-60.0..=6.0)
                                .speed(0.1)
                                .suffix(" dB"),
                        )
                        .changed()
                    {
                        cue.volume = db_to_vol(db_val);
                        volume_changed = true;
                    }
                });

                Self::field(ui, "Fade In", |ui| {
                    ui.add(
                        DragValue::new(&mut cue.fade_in_secs)
                            .range(0.0..=60.0)
                            .speed(0.05)
                            .suffix(" s"),
                    );
                });

                Self::field(ui, "Fade Out", |ui| {
                    ui.add(
                        DragValue::new(&mut cue.fade_out_secs)
                            .range(0.0..=60.0)
                            .speed(0.05)
                            .suffix(" s"),
                    );
                });

                Self::field(ui, "Fade Curve", |ui| {
                    egui::ComboBox::from_id_salt("playback_fade_curve")
                        .selected_text(format!("{:?}", cue.fade_curve))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut cue.fade_curve, FadeCurve::Linear, "Linear");
                            ui.selectable_value(
                                &mut cue.fade_curve,
                                FadeCurve::Logarithmic,
                                "Logarithmic",
                            );
                            ui.selectable_value(&mut cue.fade_curve, FadeCurve::SCurve, "S-Curve");
                            ui.selectable_value(
                                &mut cue.fade_curve,
                                FadeCurve::Exponential,
                                "Exponential",
                            );
                            ui.selectable_value(&mut cue.fade_curve, FadeCurve::Custom, "Custom");
                        });
                });

                if cue.fade_curve == FadeCurve::Custom {
                    Self::custom_fade_editor(ui, &mut cue.custom_fade_points, true);
                }
            });

            Self::section(&mut cols[1], "Time & Pitch", |ui| {
                Self::field(ui, "Playback speed", |ui| {
                    if ui
                        .add(
                            DragValue::new(&mut cue.time_pitch.speed)
                                .range(0.1..=4.0)
                                .speed(0.1)
                                .fixed_decimals(1)
                                .suffix("×"),
                        )
                        .changed()
                    {
                        cue.time_pitch.speed =
                            ((cue.time_pitch.speed * 10.0).round() / 10.0).clamp(0.1, 4.0);
                        time_pitch_changed = true;
                    }
                });
                Self::field(ui, "Speed mode", |ui| {
                    if ui
                        .checkbox(
                            &mut cue.time_pitch.preserve_pitch_on_speed_change,
                            "Preserve pitch (time-stretch)",
                        )
                        .on_hover_text(
                            "On: speed changes duration but keeps pitch. Off: pitch follows speed (varispeed).",
                        )
                        .changed()
                    {
                        time_pitch_changed = true;
                    }
                });

                let mut semitones = (cue.time_pitch.pitch_cents / 100.0).trunc() as i32;
                let mut cents = (cue.time_pitch.pitch_cents - semitones as f32 * 100.0) as i32;
                Self::field(ui, "Pitch shift", |ui| {
                    ui.horizontal(|ui| {
                        let semitones_changed = ui
                            .add(
                                DragValue::new(&mut semitones)
                                    .range(-24..=24)
                                    .speed(1)
                                    .suffix(" st"),
                            )
                            .changed();
                        let cents_changed = ui
                            .add(
                                DragValue::new(&mut cents)
                                    .range(-99..=99)
                                    .speed(1)
                                    .suffix(" cents"),
                            )
                            .changed();
                        if semitones_changed || cents_changed {
                            cue.time_pitch.pitch_cents =
                                ((semitones * 100 + cents.clamp(-99, 99)) as f32)
                                    .clamp(-2400.0, 2400.0);
                            time_pitch_changed = true;
                        }
                    });
                });
                Self::field(ui, "Pitch mode", |ui| {
                    if ui
                        .checkbox(
                            &mut cue.time_pitch.pitch_shift_changes_speed,
                            "Varispeed: pitch shift also changes duration",
                        )
                        .on_hover_text(
                            "On: pitch shift also changes playback speed and duration. Off: pitch changes without changing duration.",
                        )
                        .changed()
                    {
                        time_pitch_changed = true;
                    }
                });
                ui.label(
                    RichText::new("Speed: 0.1× steps · Pitch: semitones + fine cents. Waveform automation is edited in the Waveform tab.")
                        .color(Theme::text_dim())
                        .size(10.5),
                );
            });
        });

        (volume_changed, time_pitch_changed)
    }

    fn source_channel_short_name(input_channels: usize, source_index: usize) -> String {
        match input_channels {
            1 => "Mono".to_string(),
            2 => (if source_index == 0 { "L" } else { "R" }).to_string(),
            6 => ["L", "R", "C", "LFE", "Ls", "Rs"]
                .get(source_index)
                .copied()
                .unwrap_or("Ch")
                .to_string(),
            8 => ["L", "R", "C", "LFE", "Ls", "Rs", "Lb", "Rb"]
                .get(source_index)
                .copied()
                .unwrap_or("Ch")
                .to_string(),
            _ => format!("Ch {}", source_index + 1),
        }
    }

    /// Per-track output routing. Each hardware output is a vertical strip with
    /// its own output trim and a checkbox for every source channel, allowing
    /// fan-out and downmixing in one compact horizontal matrix.
    pub fn show_io_routing(ui: &mut Ui, cue: Option<&mut Cue>, audio: &mut AudioEngine) -> bool {
        let Some(cue) = cue else {
            ui.label(
                RichText::new("Select an audio cue to configure its channel routing")
                    .color(Theme::text_dim()),
            );
            return false;
        };
        if !matches!(&cue.cue_type, CueType::Audio) {
            ui.label(
                RichText::new("Select an audio cue to configure its channel routing")
                    .color(Theme::text_dim()),
            );
            return false;
        }
        if cue.audio_channels == 0 {
            ui.label(
                RichText::new("Audio channel count has not been detected for this file yet.")
                    .color(Theme::text_dim()),
            );
            return false;
        }

        let mut changed = false;
        let cue_id = cue.id;
        let global_device = format!("{} — {}", audio.backend.short_label(), audio.current_device);
        let mut selected_device = cue.io.output_device.clone();
        let mut selected_backend = selected_device.as_ref().map(|_| {
            cue.io
                .output_backend
                .or_else(|| {
                    cue.io
                        .output_device_id
                        .as_deref()
                        .and_then(|id| audio.device_registry.find_by_id(id))
                        .map(|endpoint| endpoint.backend)
                })
                .unwrap_or(audio.backend)
        });
        let mut selected_device_id = cue.io.output_device_id.clone();

        // Migrate older projects that stored only the backend/name pair.
        if selected_device_id.is_none() {
            if let (Some(device_name), Some(backend)) =
                (selected_device.as_deref(), selected_backend)
            {
                selected_device_id = audio.registered_endpoint_id(backend, device_name);
            }
        }

        let selected_endpoint_label = selected_device.as_ref().map_or_else(
            || format!("Global default ({})", global_device),
            |device_name| {
                selected_device_id
                    .as_deref()
                    .and_then(|id| audio.device_registry.find_by_id(id))
                    .map(|endpoint| endpoint.label())
                    .unwrap_or_else(|| {
                        format!(
                            "{} — {} (not in registry)",
                            selected_backend.unwrap_or(audio.backend).short_label(),
                            device_name
                        )
                    })
            },
        );

        ui.horizontal(|ui| {
            ui.label("Output device");
            egui::ComboBox::from_id_salt(("cue_output_device", cue_id))
                .width(350.0)
                .selected_text(&selected_endpoint_label)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(
                            selected_device.is_none(),
                            format!("Global default ({})", global_device),
                        )
                        .clicked()
                    {
                        selected_device = None;
                        selected_backend = None;
                        selected_device_id = None;
                    }

                    for endpoint in audio.device_registry.endpoints().to_vec() {
                        let selected = selected_device_id.as_deref() == Some(endpoint.id.as_str())
                            || (selected_device_id.is_none()
                                && selected_device.as_deref()
                                    == Some(endpoint.device_name.as_str())
                                && selected_backend == Some(endpoint.backend));
                        let label = endpoint.label();
                        ui.add_enabled_ui(endpoint.is_available(), |ui| {
                            let response = ui.selectable_label(selected, label);
                            if response.clicked() {
                                selected_device = Some(endpoint.device_name.clone());
                                selected_backend = Some(endpoint.backend);
                                selected_device_id = Some(endpoint.id.clone());
                            }
                        });
                    }
                });
        });

        if let Some(endpoint_id) = selected_device_id.as_deref() {
            if let Some(resolved_name) = audio.resolve_registered_endpoint_name(endpoint_id) {
                selected_device = Some(resolved_name);
            }
        } else if selected_backend == Some(audio.backend) {
            if let Some(device_name) = selected_device.as_deref() {
                if let Some(resolved_name) = audio.resolve_output_device_name(device_name) {
                    selected_device = Some(resolved_name);
                }
            }
        }

        if selected_device != cue.io.output_device
            || selected_backend != cue.io.output_backend
            || selected_device_id != cue.io.output_device_id
        {
            let endpoint_available = selected_device.as_deref().is_some_and(|device_name| {
                audio.registered_endpoint_available(
                    selected_backend.unwrap_or(audio.backend),
                    selected_device_id.as_deref(),
                    device_name,
                )
            });
            if endpoint_available && selected_backend == Some(audio.backend) {
                if let Some(device_name) = selected_device.as_deref() {
                    audio.refresh_output_device_info(device_name);
                }
            }
            cue.io.output_device = selected_device;
            cue.io.output_backend = selected_backend;
            cue.io.output_device_id = selected_device_id;
            changed = true;
        }

        if let Some(device_name) = cue.io.output_device.as_deref() {
            let route_backend = cue.io.output_backend.unwrap_or(audio.backend);
            if !audio.registered_endpoint_available(
                route_backend,
                cue.io.output_device_id.as_deref(),
                device_name,
            ) {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    format!(
                        "{} — {} is currently unavailable. The saved route is retained and will become available when the device reconnects.",
                        route_backend.short_label(),
                        device_name
                    ),
                );
                return changed;
            }
            if route_backend != audio.backend {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    format!(
                        "This route is saved as {} — {}. Switch the global output to this backend to edit its channel matrix. Simultaneous cross-backend playback becomes available in Stage 4.",
                        route_backend.short_label(),
                        device_name
                    ),
                );
                return changed;
            }
            if route_backend == AudioBackend::Asio && device_name != audio.current_device.as_str() {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    format!(
                        "This route is saved as ASIO — {}. Only one ASIO driver can be active in this process. Switch the global output to this device to edit its matrix and play; simultaneous ASIO drivers arrive in Stage 4.",
                        device_name
                    ),
                );
                return changed;
            }
        }

        let output_channels = audio
            .output_channels_for_device(cue.io.output_device.as_deref())
            .max(1);
        let input_channels = cue.audio_channels;
        let default_route_channels = audio
            .default_route_channels_for_source(cue.io.output_device.as_deref(), input_channels);
        cue.io.ensure_channel_routes_with_defaults(
            input_channels,
            output_channels,
            &default_route_channels,
        );
        let output_order = audio.output_channel_display_order(cue.io.output_device.as_deref());

        let format = match input_channels {
            1 => "Mono",
            2 => "Stereo",
            6 => "5.1",
            8 => "7.1",
            _ => "Multichannel",
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!(
                    "{} · {} source channels · {} hardware outputs",
                    format, input_channels, output_channels
                ))
                .strong()
                .color(Theme::accent()),
            );
            ui.label(
                RichText::new("Select one or more source channels for each output.")
                    .color(Theme::text_dim())
                    .size(10.5),
            );
        });
        ui.add_space(6.0);

        // One horizontal scroll area owns the entire matrix. The master is the
        // first item in that content, so stale nested-scroll offsets cannot leave
        // a large empty region before the faders.
        let mut output_gains_db = cue.io.output_gains_db.clone();
        let routing_matrix_height = (240.0 + input_channels as f32 * 24.0).max(270.0);
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), routing_matrix_height),
            egui::Layout::top_down(egui::Align::Min),
            |matrix_ui| {
                egui::ScrollArea::horizontal()
                    .id_salt((
                        "io_routing_matrix_v5",
                        cue_id,
                        cue.io.output_device.clone(),
                        input_channels,
                        output_channels,
                    ))
                    .auto_shrink([false, false])
                    .show(matrix_ui, |ui| {
                        ui.set_min_height(routing_matrix_height);
                        ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                            // Master is a full-width channel strip like OUT 1/2/... .
                            // Its checkboxes enable/disable individual source channels
                            // before they are routed to hardware outputs.
                            Self::mixer_strip(ui, routing_matrix_height, |ui| {
                                ui.label(RichText::new("MASTER").strong().color(Theme::text()));
                                ui.add_sized(
                                    [108.0, 28.0],
                                    Label::new(
                                        RichText::new("All source channels")
                                            .color(Theme::text_dim())
                                            .size(9.5),
                                    )
                                    .wrap(),
                                );
                                ui.label(
                                    RichText::new("Cue trim").color(Theme::text_dim()).size(9.5),
                                );
                                let mut master_db = vol_to_db(cue.volume);
                                let slider = Slider::new(&mut master_db, -60.0..=6.0)
                                    .vertical()
                                    .show_value(false);
                                let slider_response = ui.add_sized([30.0, 116.0], slider);
                                let reset_to_zero =
                                    Self::fader_double_clicked(ui, &slider_response);
                                let slider_changed = slider_response.changed() || reset_to_zero;
                                if reset_to_zero {
                                    master_db = 0.0;
                                }
                                if slider_changed {
                                    cue.volume = db_to_vol(master_db);
                                    changed = true;
                                }
                                ui.horizontal(|ui| {
                                    if Self::gain_db_input(
                                        ui,
                                        Id::new(("io_master_gain_db", cue_id)),
                                        &mut master_db,
                                        -60.0,
                                        6.0,
                                        slider_changed,
                                    ) {
                                        cue.volume = db_to_vol(master_db);
                                        changed = true;
                                    }
                                });
                                ui.separator();

                                for source_index in 0..input_channels {
                                    let label = Self::source_channel_short_name(
                                        input_channels,
                                        source_index,
                                    );
                                    let route = &mut cue.io.channel_routes[source_index];
                                    let mut active = route.master_enabled;
                                    if ui.checkbox(&mut active, label).changed() {
                                        route.master_enabled = active;
                                        changed = true;
                                    }
                                }
                            });

                            ui.separator();

                            for (logical_index, output_index) in
                                output_order.iter().copied().enumerate()
                            {
                                let Some(output_gain_db) = output_gains_db.get_mut(output_index)
                                else {
                                    continue;
                                };
                                Self::mixer_strip(ui, routing_matrix_height, |ui| {
                                    ui.label(
                                        RichText::new(format!("OUT {}", logical_index + 1))
                                            .strong()
                                            .color(Theme::text()),
                                    );
                                    ui.add_sized(
                                        [108.0, 28.0],
                                        Label::new(
                                            RichText::new(audio.output_channel_label(
                                                cue.io.output_device.as_deref(),
                                                output_index,
                                            ))
                                            .color(Theme::text_dim())
                                            .size(9.5),
                                        )
                                        .wrap(),
                                    );
                                    ui.label(
                                        RichText::new("Cue trim")
                                            .color(Theme::text_dim())
                                            .size(9.5),
                                    );
                                    let output_slider = Slider::new(output_gain_db, -60.0..=18.0)
                                        .vertical()
                                        .show_value(false);
                                    let slider_response =
                                        ui.add_sized([30.0, 116.0], output_slider);
                                    let reset_to_zero =
                                        Self::fader_double_clicked(ui, &slider_response);
                                    let slider_changed = slider_response.changed() || reset_to_zero;
                                    if reset_to_zero {
                                        *output_gain_db = 0.0;
                                    }
                                    changed |= slider_changed;
                                    ui.horizontal(|ui| {
                                        changed |= Self::gain_db_input(
                                            ui,
                                            Id::new(("io_output_gain_db", cue_id, output_index)),
                                            output_gain_db,
                                            -60.0,
                                            18.0,
                                            slider_changed,
                                        );
                                    });
                                    ui.separator();

                                    for source_index in 0..input_channels {
                                        let source_name = Self::source_channel_short_name(
                                            input_channels,
                                            source_index,
                                        );
                                        let route = &mut cue.io.channel_routes[source_index];
                                        let mut active = route.active
                                            && route.output_channels.contains(&output_index);
                                        if ui.checkbox(&mut active, source_name).changed() {
                                            if active {
                                                if !route.output_channels.contains(&output_index) {
                                                    route.output_channels.push(output_index);
                                                }
                                            } else {
                                                route
                                                    .output_channels
                                                    .retain(|index| *index != output_index);
                                            }
                                            route.output_channels.sort_unstable();
                                            route.output_channels.dedup();
                                            route.active = !route.output_channels.is_empty();
                                            if let Some(primary) =
                                                route.output_channels.first().copied()
                                            {
                                                route.output_channel = primary;
                                            }
                                            changed = true;
                                        }
                                    }
                                });
                            }
                        });
                    });
            },
        );
        // The faders edit a local copy inside the nested UI closures. Commit
        // it when a value actually changes; the old flag was never set, which
        // meant the edited gains could be discarded every frame.
        if output_gains_db != cue.io.output_gains_db {
            cue.io.output_gains_db = output_gains_db;
            changed = true;
        }

        // Keep legacy L/R fields synchronized for older project versions.
        if let Some(left) = cue.io.channel_routes.first() {
            cue.io.left_enabled = left.active;
            cue.io.left_gain_db = left.gain_db;
        }
        if let Some(right) = cue.io.channel_routes.get(1) {
            cue.io.right_enabled = right.active;
            cue.io.right_gain_db = right.gain_db;
        }

        changed
    }

    /// Dedicated Trim mixer. Its layout intentionally mirrors I/O Routing:
    /// a master strip on the left, followed by one vertical source-trim strip
    /// per input channel in a horizontally scrolling row.
    pub fn show_source_channel_trims(
        ui: &mut Ui,
        cue: Option<&mut Cue>,
        audio: &AudioEngine,
    ) -> bool {
        let Some(cue) = cue else {
            ui.label(
                RichText::new("Select an audio cue to configure source channel trims")
                    .color(Theme::text_dim()),
            );
            return false;
        };
        if !matches!(&cue.cue_type, CueType::Audio) {
            ui.label(
                RichText::new("Select an audio cue to configure source channel trims")
                    .color(Theme::text_dim()),
            );
            return false;
        }
        if cue.audio_channels == 0 {
            ui.label(
                RichText::new("Audio channel count has not been detected for this file yet.")
                    .color(Theme::text_dim()),
            );
            return false;
        }

        let input_channels = cue.audio_channels;
        let output_channels = audio
            .output_channels_for_device(cue.io.output_device.as_deref())
            .max(1);
        let default_routes = audio
            .default_route_channels_for_source(cue.io.output_device.as_deref(), input_channels);
        let previous_routes = cue.io.channel_routes.clone();
        cue.io.ensure_channel_routes_with_defaults(
            input_channels,
            output_channels,
            &default_routes,
        );

        let format = match input_channels {
            1 => "Mono",
            2 => "Stereo",
            6 => "5.1",
            8 => "7.1",
            _ => "Multichannel",
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!("{} · {} source channels", format, input_channels))
                    .strong()
                    .color(Theme::accent()),
            );
            ui.label(
                RichText::new(
                    "Adjust the master level and each source channel before output routing.",
                )
                .color(Theme::text_dim())
                .size(10.5),
            );
        });
        ui.add_space(6.0);

        let cue_id = cue.id;
        let mut changed = cue.io.channel_routes != previous_routes;
        let matrix_height = (240.0 + input_channels as f32 * 24.0).max(270.0);
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), matrix_height),
            egui::Layout::top_down(egui::Align::Min),
            |matrix_ui| {
                egui::ScrollArea::horizontal()
                    .id_salt(("source_trim_matrix_v2", cue_id, input_channels))
                    .auto_shrink([false, false])
                    .show(matrix_ui, |ui| {
                        ui.set_min_height(matrix_height);
                        ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                            // Match the I/O Routing master strip exactly: same
                            // card width, vertical fader, readout and channel toggles.
                            Self::mixer_strip(ui, matrix_height, |ui| {
                                ui.label(RichText::new("MASTER").strong().color(Theme::text()));
                                ui.add_sized(
                                    [108.0, 28.0],
                                    Label::new(
                                        RichText::new("All source channels")
                                            .color(Theme::text_dim())
                                            .size(9.5),
                                    )
                                    .wrap(),
                                );
                                ui.label(
                                    RichText::new("Cue trim").color(Theme::text_dim()).size(9.5),
                                );
                                let mut master_db = vol_to_db(cue.volume);
                                let master_slider = Slider::new(&mut master_db, -60.0..=6.0)
                                    .vertical()
                                    .show_value(false);
                                let slider_response = ui.add_sized([30.0, 116.0], master_slider);
                                let reset_to_zero =
                                    Self::fader_double_clicked(ui, &slider_response);
                                let slider_changed = slider_response.changed() || reset_to_zero;
                                if reset_to_zero {
                                    master_db = 0.0;
                                }
                                if slider_changed {
                                    cue.volume = db_to_vol(master_db);
                                    changed = true;
                                }
                                ui.horizontal(|ui| {
                                    if Self::gain_db_input(
                                        ui,
                                        Id::new(("trim_master_gain_db", cue_id)),
                                        &mut master_db,
                                        -60.0,
                                        6.0,
                                        slider_changed,
                                    ) {
                                        cue.volume = db_to_vol(master_db);
                                        changed = true;
                                    }
                                });
                                ui.separator();

                                for source_index in 0..input_channels {
                                    let label = Self::source_channel_short_name(
                                        input_channels,
                                        source_index,
                                    );
                                    let route = &mut cue.io.channel_routes[source_index];
                                    let mut master_enabled = route.master_enabled;
                                    if ui.checkbox(&mut master_enabled, label).changed() {
                                        route.master_enabled = master_enabled;
                                        changed = true;
                                    }
                                }
                            });

                            ui.separator();

                            // Each source gets the same vertical-strip treatment
                            // as an OUT strip in I/O Routing, arranged left-to-right.
                            for source_index in 0..input_channels {
                                let source_label =
                                    Self::source_channel_short_name(input_channels, source_index);
                                let route = &mut cue.io.channel_routes[source_index];
                                Self::mixer_strip(ui, matrix_height, |ui| {
                                    ui.label(
                                        RichText::new(format!("SOURCE {}", source_index + 1))
                                            .strong()
                                            .color(Theme::text()),
                                    );
                                    ui.add_sized(
                                        [108.0, 28.0],
                                        Label::new(
                                            RichText::new(source_label.clone())
                                                .color(Theme::text_dim())
                                                .size(9.5),
                                        )
                                        .wrap(),
                                    );
                                    ui.label(
                                        RichText::new("Input trim")
                                            .color(Theme::text_dim())
                                            .size(9.5),
                                    );
                                    let source_slider =
                                        Slider::new(&mut route.gain_db, -60.0..=18.0)
                                            .vertical()
                                            .show_value(false);
                                    let slider_response =
                                        ui.add_sized([30.0, 116.0], source_slider);
                                    let reset_to_zero =
                                        Self::fader_double_clicked(ui, &slider_response);
                                    let slider_changed = slider_response.changed() || reset_to_zero;
                                    if reset_to_zero {
                                        route.gain_db = 0.0;
                                    }
                                    changed |= slider_changed;
                                    ui.horizontal(|ui| {
                                        changed |= Self::gain_db_input(
                                            ui,
                                            Id::new(("trim_source_gain_db", cue_id, source_index)),
                                            &mut route.gain_db,
                                            -60.0,
                                            18.0,
                                            slider_changed,
                                        );
                                    });
                                    ui.separator();
                                    let mut active = route.active;
                                    if ui.checkbox(&mut active, "Active").changed() {
                                        route.active = active;
                                        changed = true;
                                    }
                                });
                            }
                        });
                    });
            },
        );

        // Keep the legacy stereo fields synchronized for older project versions.
        if let Some(left) = cue.io.channel_routes.first() {
            cue.io.left_enabled = left.active;
            cue.io.left_gain_db = left.gain_db;
        }
        if let Some(right) = cue.io.channel_routes.get(1) {
            cue.io.right_enabled = right.active;
            cue.io.right_gain_db = right.gain_db;
        }

        changed
    }

    pub fn show_midi(ui: &mut Ui, cue: &mut Cue, midi_out: &mut MidiOutputManager) {
        let CueType::Midi {
            ref mut channel,
            ref mut message,
            ref mut raw_hex,
        } = cue.cue_type
        else {
            return;
        };

        ui.columns(2, |cols| {
            Self::section(&mut cols[0], "MIDI Output & Message Parameters", |ui| {
                // Выбор выходного MIDI-порта
                let port_label = midi_out
                    .current_port
                    .clone()
                    .unwrap_or_else(|| "No MIDI Output Port".to_string());
                Self::field(ui, "MIDI Out", |ui| {
                    egui::ComboBox::from_id_salt("midi_out_port")
                        .width(200.0)
                        .selected_text(port_label)
                        .show_ui(ui, |ui| {
                            for p in &midi_out.port_names.clone() {
                                if ui
                                    .selectable_label(
                                        midi_out.current_port.as_deref() == Some(p.as_str()),
                                        p,
                                    )
                                    .clicked()
                                {
                                    midi_out.refresh_and_connect(Some(p));
                                }
                            }
                        });
                });

                Self::field(ui, "Channel", |ui| {
                    ui.add(DragValue::new(channel).range(1..=16));
                });

                Self::field(ui, "Command", |ui| {
                    egui::ComboBox::from_id_salt("midi_msg_kind")
                        .selected_text(message.label())
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(
                                    matches!(message, MidiMessageKind::NoteOn { .. }),
                                    "Note On",
                                )
                                .clicked()
                            {
                                *message = MidiMessageKind::NoteOn {
                                    note: 60,
                                    velocity: 100,
                                };
                            }
                            if ui
                                .selectable_label(
                                    matches!(message, MidiMessageKind::NoteOff { .. }),
                                    "Note Off",
                                )
                                .clicked()
                            {
                                *message = MidiMessageKind::NoteOff {
                                    note: 60,
                                    velocity: 0,
                                };
                            }
                            if ui
                                .selectable_label(
                                    matches!(message, MidiMessageKind::ControlChange { .. }),
                                    "Control Change (CC)",
                                )
                                .clicked()
                            {
                                *message = MidiMessageKind::ControlChange { cc: 7, value: 100 };
                            }
                            if ui
                                .selectable_label(
                                    matches!(message, MidiMessageKind::ProgramChange { .. }),
                                    "Program Change",
                                )
                                .clicked()
                            {
                                *message = MidiMessageKind::ProgramChange { program: 0 };
                            }
                            if ui
                                .selectable_label(
                                    matches!(message, MidiMessageKind::PitchBend { .. }),
                                    "Pitch Bend",
                                )
                                .clicked()
                            {
                                *message = MidiMessageKind::PitchBend { value: 0 };
                            }
                            if ui
                                .selectable_label(
                                    matches!(message, MidiMessageKind::Raw(_)),
                                    "Raw MIDI Code (Hex)",
                                )
                                .clicked()
                            {
                                if raw_hex.is_empty() {
                                    *raw_hex = "90 3C 7F".to_string();
                                }
                                *message =
                                    MidiMessageKind::Raw(MidiMessageKind::parse_hex(raw_hex));
                            }
                        });
                });

                match message {
                    MidiMessageKind::NoteOn { note, velocity }
                    | MidiMessageKind::NoteOff { note, velocity } => {
                        Self::field(ui, "Note", |ui| {
                            ui.add(DragValue::new(note).range(0..=127));
                        });
                        Self::field(ui, "Velocity", |ui| {
                            ui.add(DragValue::new(velocity).range(0..=127));
                        });
                    }
                    MidiMessageKind::ControlChange { cc, value } => {
                        Self::field(ui, "CC #", |ui| {
                            ui.add(DragValue::new(cc).range(0..=127));
                        });
                        Self::field(ui, "Value", |ui| {
                            ui.add(DragValue::new(value).range(0..=127));
                        });
                    }
                    MidiMessageKind::ProgramChange { program } => {
                        Self::field(ui, "Program", |ui| {
                            ui.add(DragValue::new(program).range(0..=127));
                        });
                    }
                    MidiMessageKind::PitchBend { value } => {
                        Self::field(ui, "Bend", |ui| {
                            ui.add(DragValue::new(value).range(-8192..=8191));
                        });
                    }
                    MidiMessageKind::Raw(_) => {}
                }
            });

            Self::section(&mut cols[1], "Ready MIDI Code (Hex / SysEx)", |ui| {
                ui.label(
                    RichText::new(
                        "Enter raw MIDI bytes in Hex (e.g. 90 3C 7F or F0 7F 7F 06 02 F7):",
                    )
                    .color(Theme::text_dim())
                    .size(11.0),
                );
                if ui
                    .add(
                        TextEdit::multiline(raw_hex)
                            .font(TextStyle::Monospace)
                            .desired_width(260.0)
                            .desired_rows(2),
                    )
                    .changed()
                {
                    let parsed = MidiMessageKind::parse_hex(raw_hex);
                    if matches!(message, MidiMessageKind::Raw(_)) {
                        *message = MidiMessageKind::Raw(parsed);
                    }
                }

                let bytes_to_send =
                    if !raw_hex.trim().is_empty() && matches!(message, MidiMessageKind::Raw(_)) {
                        MidiMessageKind::parse_hex(raw_hex)
                    } else {
                        message.to_bytes(*channel)
                    };

                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("Compiled bytes: {:02X?}", bytes_to_send))
                            .monospace()
                            .size(11.0)
                            .color(Theme::accent()),
                    );
                    if ui.button("🎹 Send MIDI Now").clicked() {
                        let _ = midi_out.send_bytes(&bytes_to_send);
                    }
                });
            });
        });
    }

    fn fade_curve_preview(ui: &mut Ui, curve: FadeCurve, fade_in: bool) {
        let (rect, _) = ui.allocate_exact_size(vec2(300.0, 72.0), Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 3.0, Theme::bg_dark());
        painter.rect_stroke(
            rect,
            3.0,
            Stroke::new(1.0_f32, Theme::border()),
            StrokeKind::Inside,
        );
        let points: Vec<Pos2> = (0..=48)
            .map(|index| {
                let t = index as f32 / 48.0;
                let progress = curve.apply(t);
                let level = if fade_in { progress } else { 1.0 - progress };
                pos2(
                    rect.left() + t * rect.width(),
                    rect.bottom() - level * rect.height(),
                )
            })
            .collect();
        painter.add(Shape::line(points, Stroke::new(2.0_f32, Theme::accent())));
    }

    fn custom_fade_editor(ui: &mut Ui, points: &mut Vec<[f32; 2]>, fade_in: bool) {
        ui.add_space(4.0);
        ui.label(
            RichText::new("Draw the fade curve by dragging inside the graph")
                .color(Theme::text_dim())
                .size(11.0),
        );

        let (rect, response) = ui.allocate_exact_size(vec2(300.0, 118.0), Sense::drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 3.0, Theme::bg_dark());
        painter.rect_stroke(
            rect,
            3.0,
            Stroke::new(1.0_f32, Theme::border()),
            StrokeKind::Inside,
        );

        for step in 1..4 {
            let t = step as f32 / 4.0;
            let x = rect.left() + rect.width() * t;
            let y = rect.bottom() - rect.height() * t;
            painter.line_segment(
                [pos2(x, rect.top()), pos2(x, rect.bottom())],
                Stroke::new(1.0_f32, Theme::border()),
            );
            painter.line_segment(
                [pos2(rect.left(), y), pos2(rect.right(), y)],
                Stroke::new(1.0_f32, Theme::border()),
            );
        }

        if response.dragged() {
            if let Some(pointer) = response.interact_pointer_pos() {
                let x = ((pointer.x - rect.left()) / rect.width()).clamp(0.01, 0.99);
                let displayed_level =
                    (1.0 - (pointer.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
                let y = if fade_in {
                    displayed_level
                } else {
                    1.0 - displayed_level
                };
                if points.len() < 2 {
                    *points = vec![[0.0, 0.0], [1.0, 1.0]];
                }
                points.retain(|point| {
                    point[0] <= 0.0 || point[0] >= 1.0 || (point[0] - x).abs() > 0.012
                });
                points.push([x, y]);
                points.sort_by(|a, b| a[0].total_cmp(&b[0]));
                points[0] = [0.0, 0.0];
                if let Some(last) = points.last_mut() {
                    *last = [1.0, 1.0];
                }
                ui.ctx().request_repaint();
            }
        }

        let curve_points: Vec<Pos2> = points
            .iter()
            .map(|point| {
                pos2(
                    rect.left() + point[0].clamp(0.0, 1.0) * rect.width(),
                    rect.bottom()
                        - (if fade_in { point[1] } else { 1.0 - point[1] }).clamp(0.0, 1.0)
                            * rect.height(),
                )
            })
            .collect();
        if curve_points.len() >= 2 {
            painter.add(Shape::line(
                curve_points,
                Stroke::new(2.0_f32, Theme::accent()),
            ));
        }

        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{} control points", points.len()))
                    .color(Theme::text_dim())
                    .size(10.0),
            );
            if ui.small_button("Reset linear").clicked() {
                *points = vec![[0.0, 0.0], [1.0, 1.0]];
            }
        });
    }

    fn section(ui: &mut Ui, title: &str, content: impl FnOnce(&mut Ui)) {
        ui.label(
            RichText::new(title)
                .color(Theme::accent())
                .size(11.5)
                .strong(),
        );
        ui.indent(title, |ui| {
            content(ui);
        });
    }

    /// A mixer channel card. Fixed inner dimensions keep MASTER, OUT and SOURCE
    /// strips identical in width, height, border and spacing across both tabs.
    fn mixer_strip(ui: &mut Ui, height: f32, contents: impl FnOnce(&mut Ui)) {
        Frame::group(ui.style()).show(ui, |strip_ui| {
            strip_ui.set_min_width(116.0);
            strip_ui.set_max_width(116.0);
            strip_ui.set_min_height((height - 16.0).max(180.0));
            strip_ui.vertical_centered(contents);
        });
    }

    /// Detect a double-click anywhere on the visible fader hit area.
    ///
    /// egui's Slider response only covers the narrow interactive widget itself.
    /// In practice that makes a double-click reset difficult because the thumb
    /// moves between clicks. Expand the hit target horizontally while keeping
    /// the action confined to the fader track (not the dB editor or checkboxes).
    fn fader_double_clicked(ui: &Ui, slider_response: &Response) -> bool {
        if slider_response.double_clicked() {
            return true;
        }

        ui.input(|input| {
            input.pointer.button_double_clicked(PointerButton::Primary)
                && input
                    .pointer
                    .interact_pos()
                    .map(|position| {
                        slider_response
                            .rect
                            .expand2(vec2(12.0, 3.0))
                            .contains(position)
                    })
                    .unwrap_or(false)
        })
    }

    /// Editable dB readout. Click to type a value; Enter or focus loss commits it.
    /// `sync_text` refreshes the edit buffer after the adjacent fader is moved.
    fn gain_db_input(
        ui: &mut Ui,
        id: Id,
        value: &mut f32,
        min_db: f32,
        max_db: f32,
        sync_text: bool,
    ) -> bool {
        let text_id = id.with("gain_db_text");
        if sync_text {
            ui.ctx().data_mut(|data| {
                data.insert_temp(text_id, format!("{:+.1}", *value));
            });
        }
        let mut text = ui
            .ctx()
            .data(|data| data.get_temp::<String>(text_id))
            .unwrap_or_else(|| format!("{:+.1}", *value));
        let response = ui.add_sized(
            [62.0, 22.0],
            TextEdit::singleline(&mut text)
                .desired_width(62.0)
                .font(FontId::monospace(10.0)),
        );
        ui.label(RichText::new("dB").size(10.0).color(Theme::text_dim()));

        let mut changed = false;
        let commit = response.lost_focus()
            || (response.has_focus() && ui.input(|input| input.key_pressed(Key::Enter)));
        if commit {
            let normalized = text.trim().replace(',', ".");
            if let Ok(parsed) = normalized.parse::<f32>() {
                if parsed.is_finite() {
                    let next = parsed.clamp(min_db, max_db);
                    changed = (*value - next).abs() > f32::EPSILON;
                    *value = next;
                }
            }
            text = format!("{:+.1}", *value);
        }
        ui.ctx().data_mut(|data| data.insert_temp(text_id, text));
        changed
    }

    fn field(ui: &mut Ui, label: &str, content: impl FnOnce(&mut Ui)) {
        ui.horizontal(|ui| {
            ui.add_sized(
                [76.0, 20.0],
                Label::new(RichText::new(label).color(Theme::text_dim()).size(11.0)),
            );
            content(ui);
        });
    }
}

fn vol_to_db(vol: f32) -> f32 {
    if vol <= 0.001 {
        -60.0
    } else {
        20.0 * vol.log10()
    }
}

fn db_to_vol(db: f32) -> f32 {
    if db <= -60.0 {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}
