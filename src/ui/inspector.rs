// src/ui/inspector.rs — вкладки нижней панели: Inspector, Playback, I/O Routing и MIDI

use crate::audio::engine::AudioEngine;
use crate::cue::{
    ContinueMode, Cue, CueColor, CueType, FadeCurve, GroupMode, MidiMessageKind,
};
use crate::midi::input::MidiOutputManager;
use crate::ui::theme::Theme;
use egui::*;
use uuid::Uuid;

pub struct Inspector;

impl Inspector {
    /// Вкладка 1: Основные свойства, тайминги Pre/Post-Wait, выбор цели (для Play/Pause/Stop/Fade) и состав группы
    pub fn show_inspector(
        ui: &mut Ui,
        cue: &mut Cue,
        audio_targets: &[(Uuid, String, std::path::PathBuf)],
        all_cues: &[(Uuid, String)],
    ) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
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
                                let fname = cue.file_path.to_string_lossy().to_string();
                                ui.label(RichText::new(fname).color(Theme::TEXT_DIM).size(11.0));
                                ui.label(
                                    RichText::new(format!(
                                        "Duration: {}",
                                        crate::ui::cue_table::format_duration(cue.duration_secs)
                                    ))
                                    .color(Theme::ACCENT)
                                    .monospace(),
                                );
                            }

                            CueType::Play { target_id }
                            | CueType::Pause { target_id }
                            | CueType::Stop { target_id } => {
                                Self::target_selector(ui, target_id, &mut cue.file_path, audio_targets);
                            }

                            CueType::Fade {
                                target_id,
                                fade_in,
                                duration_secs,
                                stop_on_finish,
                            } => {
                                Self::target_selector(ui, target_id, &mut cue.file_path, audio_targets);
                                ui.horizontal(|ui| {
                                    ui.selectable_value(fade_in, true, "↗ Fade In");
                                    ui.selectable_value(fade_in, false, "↘ Fade Out");
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Duration:");
                                    ui.add(
                                        DragValue::new(duration_secs)
                                            .range(0.1..=120.0)
                                            .speed(0.1)
                                            .suffix(" s"),
                                    );
                                });
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
            ).color(Theme::TEXT_DIM).size(11.0));
        }
        GroupMode::Playlist => {
            ui.label(RichText::new(
                "Playlist: plays every child sequentially, waiting for each to finish. \
                 Fade settings below are applied to all children during playback."
            ).color(Theme::TEXT_DIM).size(11.0));

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
    )).color(Theme::ACCENT).size(11.0));

    // Список детей только для справки (без чекбоксов)
    egui::ScrollArea::vertical().max_height(90.0).show(ui, |ui| {
        for (cid, clabel) in all_cues {
            if *cid == cue_id { continue; }
            if children.contains(cid) {
                ui.label(RichText::new(format!("  • {}", clabel))
                    .color(Theme::TEXT_DIM).size(11.0));
            }
        }
    });
}

                            CueType::Midi { .. } => {
                                ui.label(
                                    RichText::new("Switch to the '🎹 MIDI' tab to configure MIDI parameters and raw hex code.")
                                        .color(Theme::ACCENT)
                                        .size(11.5),
                                );
                            }
                        }
                    });
                });
            });
    }

    fn target_selector(
        ui: &mut Ui,
        target_id: &mut Option<Uuid>,
        cue_file_path: &mut std::path::PathBuf,
        audio_targets: &[(Uuid, String, std::path::PathBuf)],
    ) {
        let selected_label = target_id
            .and_then(|id| audio_targets.iter().find(|(tid, _, _)| *tid == id))
            .map(|(_, l, _)| l.as_str())
            .unwrap_or("All / Select project audio cue…");

        ui.label(RichText::new("Target Audio in Project:").color(Theme::TEXT_DIM).size(11.0));
        egui::ComboBox::from_id_salt("target_audio_cue")
            .width(220.0)
            .selected_text(selected_label)
            .show_ui(ui, |ui| {
                if ui.selectable_label(target_id.is_none(), "None / All Active").clicked() {
                    *target_id = None;
                }
                for (tid, label, path) in audio_targets {
                    if ui.selectable_label(*target_id == Some(*tid), label).clicked() {
                        *target_id = Some(*tid);
                        *cue_file_path = path.clone();
                    }
                }
            });
    }

    /// Вкладка 2: Playback
    pub fn show_playback(ui: &mut Ui, cue: &mut Cue) -> bool {
        let mut vol_changed = false;
        
        // Используем вертикальную прокрутку или вертикальный контейнер вместо horizontal_wrapped
        ui.vertical(|ui| {
            ui.add_space(4.0);
            Self::section(ui, "Levels & Envelope", |ui| {
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
                        vol_changed = true;
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
                        });
                });
            });
        });

        vol_changed
    }

    /// Вкладка 3: I/O Routing (выбор звуковой карты на Вход и Выход отдельно + маршрутизация каналов)
    pub fn show_io_routing(ui: &mut Ui, cue: Option<&mut Cue>, audio: &mut AudioEngine) {
        ui.columns(2, |cols| {
            Self::section(&mut cols[0], "Audio Interface Selection (I/O)", |ui| {
                let mut current_out = audio.current_device.clone();
                Self::field(ui, "Output Card", |ui| {
                    egui::ComboBox::from_id_salt("global_output_device")
                        .width(240.0)
                        .selected_text(&current_out)
                        .show_ui(ui, |ui| {
                            for dev in &audio.output_devices.clone() {
                                if ui
                                    .selectable_value(&mut current_out, dev.clone(), dev)
                                    .changed()
                                {
                                    audio.set_output_device(&current_out);
                                }
                            }
                        });
                });

                let mut current_in = audio.current_input_device.clone();
                Self::field(ui, "Input Card", |ui| {
                    egui::ComboBox::from_id_salt("global_input_device")
                        .width(240.0)
                        .selected_text(&current_in)
                        .show_ui(ui, |ui| {
                            for dev in &audio.input_devices.clone() {
                                if ui
                                    .selectable_value(&mut current_in, dev.clone(), dev)
                                    .changed()
                                {
                                    audio.set_input_device(&current_in);
                                }
                            }
                        });
                });
            });

            Self::section(&mut cols[1], "Channel Gains & Mute (+18dB)", |ui| {
                if let Some(cue) = cue {
                    ui.horizontal(|ui| {
                        // Левый канал
                        ui.vertical(|ui| {
                            ui.label(RichText::new("Left Ch").size(11.0).strong());
                            ui.checkbox(&mut cue.io.left_enabled, "Active");
                            ui.add_space(4.0);
                            ui.add(
                                Slider::new(&mut cue.io.left_gain_db, -60.0..=18.0)
                                    .vertical()
                                    .show_value(true)
                                    .custom_formatter(|v, _| format!("{:+.1} dB", v))
                                    .text(""),
                            );
                        });

                        ui.add_space(20.0);

                        // Правый канал
                        ui.vertical(|ui| {
                            ui.label(RichText::new("Right Ch").size(11.0).strong());
                            ui.checkbox(&mut cue.io.right_enabled, "Active");
                            ui.add_space(4.0);
                            ui.add(
                                Slider::new(&mut cue.io.right_gain_db, -60.0..=18.0)
                                    .vertical()
                                    .show_value(true)
                                    .custom_formatter(|v, _| format!("{:+.1} dB", v))
                                    .text(""),
                            );
                        });
                    });
                } else {
                    ui.label(
                        RichText::new("Select a cue to configure channel gains")
                            .color(Theme::TEXT_DIM),
                    );
                }
            });
        });
    }

    /// Вкладка 4: Настройки MIDI (отображается только когда выбрано MIDI событие)
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
                    RichText::new("Enter raw MIDI bytes in Hex (e.g. 90 3C 7F or F0 7F 7F 06 02 F7):")
                        .color(Theme::TEXT_DIM)
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

                let bytes_to_send = if !raw_hex.trim().is_empty()
                    && matches!(message, MidiMessageKind::Raw(_))
                {
                    MidiMessageKind::parse_hex(raw_hex)
                } else {
                    message.to_bytes(*channel)
                };

                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("Compiled bytes: {:02X?}", bytes_to_send))
                            .monospace()
                            .size(11.0)
                            .color(Theme::ACCENT),
                    );
                    if ui.button("🎹 Send MIDI Now").clicked() {
                        let _ = midi_out.send_bytes(&bytes_to_send);
                    }
                });
            });
        });
    }

    fn section(ui: &mut Ui, title: &str, content: impl FnOnce(&mut Ui)) {
        ui.label(RichText::new(title).color(Theme::ACCENT).size(11.5).strong());
        ui.indent(title, |ui| {
            content(ui);
        });
    }

    fn field(ui: &mut Ui, label: &str, content: impl FnOnce(&mut Ui)) {
        ui.horizontal(|ui| {
            ui.add_sized(
                [76.0, 20.0],
                Label::new(RichText::new(label).color(Theme::TEXT_DIM).size(11.0)),
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