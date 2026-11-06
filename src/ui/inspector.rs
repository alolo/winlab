// src/ui/inspector.rs — вкладки нижней панели: Inspector, Playback, I/O Routing и MIDI

use crate::audio::engine::AudioEngine;
use crate::cue::{
    ContinueMode, Cue, CueColor, CueType, FadeCurve, GroupMode, MidiMessageKind, OscCueArg,
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
        cue_targets: &[(Uuid, String, Option<std::path::PathBuf>)],
        all_cues: &[(Uuid, String)],
    ) {
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
                                    }).color(Theme::TEXT_DIM).size(10.5));
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
            ).color(Theme::TEXT_DIM).size(11.0));
        }
        GroupMode::Playlist => {
            ui.label(RichText::new(
                "Playlist: plays children sequentially. Each child waits for its pre-wait, playback completion, and post-wait."
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
        GroupMode::AllCues => {
            ui.label(RichText::new(
                "All Cues: starts every child simultaneously. Each child observes its own pre-wait; the group completes after all children and their post-waits."
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
    egui::ScrollArea::vertical()
        .id_salt(("group_children_scroll", cue_id))
        .max_height(90.0)
        .show(ui, |ui| {
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

                            CueType::Osc { address, args } => {
                                Self::field(ui, "Address", |ui| {
                                    ui.add(
                                        TextEdit::singleline(address)
                                            .hint_text("/show/cue/go")
                                            .desired_width(220.0),
                                    );
                                });
                                ui.add_space(6.0);
                                ui.label(RichText::new("Arguments").color(Theme::ACCENT).strong());
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
                                        .color(Theme::TEXT_DIM)
                                        .size(10.5),
                                );
                            }
                        }
                    });
                });
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

        ui.label(RichText::new("Target Cue / Group:").color(Theme::TEXT_DIM).size(11.0));
        egui::ComboBox::from_id_salt("target_cue_or_group")
            .width(240.0)
            .selected_text(selected_label)
            .show_ui(ui, |ui| {
                if ui.selectable_label(target_id.is_none(), "None / All Active").clicked() {
                    *target_id = None;
                }
                for (tid, label, file_path) in cue_targets {
                    if ui.selectable_label(*target_id == Some(*tid), label).clicked() {
                        *target_id = Some(*tid);
                        if let Some(path) = file_path {
                            *cue_file_path = path.clone();
                        }
                    }
                }
            });
    }

    /// Вкладка 2: Playback
    pub fn show_playback(ui: &mut Ui, cue: &mut Cue) -> bool {
        let mut vol_changed = false;

        // Use vertical scrolling instead of horizontal_wrapped for this settings section.
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
                            ui.selectable_value(&mut cue.fade_curve, FadeCurve::Custom, "Custom");
                        });
                });

                if cue.fade_curve == FadeCurve::Custom {
                    Self::custom_fade_editor(ui, &mut cue.custom_fade_points, true);
                }
            });
        });

        vol_changed
    }

    /// Per-track source channel mapping and global physical output controls.
    pub fn show_io_routing(ui: &mut Ui, cue: Option<&mut Cue>, audio: &mut AudioEngine) -> bool {
        let output_channels = audio.output_channels.max(1);
        let mut output_settings = audio.output_channel_settings();
        output_settings.resize(output_channels, crate::cue::OutputChannelSetting::default());
        output_settings.truncate(output_channels);
        let mut changed = false;
        let mut cue_opt = cue;

        ui.columns(2, |cols| {
            Self::section(&mut cols[0], "Hardware output channels", |ui| {
                ui.label(format!("Interface: {}", audio.current_device));
                ui.label(format!("Active stream outputs: {}", output_channels));
                let layouts = audio.output_channel_layouts_for_device(None);
                let layouts_text = if layouts.is_empty() { "not reported".to_string() }
                    else { layouts.iter().map(|count| format!("{} ch", count)).collect::<Vec<_>>().join(", ") };
                ui.label(format!("Driver-supported layouts: {}", layouts_text));
                ui.add_space(6.0);

                egui::ScrollArea::vertical()
                    .id_salt("io_hardware_outputs_scroll")
                    .max_height(260.0)
                    .show(ui, |ui| {
                    for (index, setting) in output_settings.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            changed |= ui.checkbox(&mut setting.active, "Active").changed();
                            ui.add_sized(
                                [78.0, 22.0],
                                Label::new(RichText::new(format!("Output {}", index + 1)).strong()),
                            );
                            let slider_changed = ui.add(
                                Slider::new(&mut setting.gain_db, -60.0..=18.0)
                                    .show_value(false),
                            ).changed();
                            if slider_changed {
                                ui.ctx().data_mut(|data| data.insert_temp(
                                    Id::new(("global_output_gain_db", index)).with("gain_db_text"),
                                    format!("{:.1}", setting.gain_db),
                                ));
                            }
                            changed |= slider_changed;
                            changed |= Self::gain_db_input(
                                ui,
                                Id::new(("global_output_gain_db", index)),
                                &mut setting.gain_db,
                            );
                            ui.label("dB");
                        });
                    }
                });
            });

            Self::section(&mut cols[1], "Track channel routing", |ui| {
                if let Some(cue) = cue_opt.as_deref_mut() {
                    if !matches!(&cue.cue_type, CueType::Audio) {
                        ui.label(RichText::new("Select an audio cue to configure its source channels")
                            .color(Theme::TEXT_DIM));
                        return;
                    }
                    let cue_id = cue.id;
                    let global_device = audio.current_device.clone();
                    let mut selected_device = cue.io.output_device.clone();
                    ui.horizontal(|ui| {
                        ui.label("Output device");
                        egui::ComboBox::from_id_salt(("cue_output_device", cue_id))
                            .width(190.0)
                            .selected_text(selected_device.clone().unwrap_or_else(|| {
                                format!("Global default ({})", global_device)
                            }))
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(
                                    selected_device.is_none(),
                                    format!("Global default ({})", global_device),
                                ).clicked() {
                                    selected_device = None;
                                }
                                for device in audio.output_devices.clone() {
                                    if ui.selectable_label(
                                        selected_device.as_deref() == Some(device.as_str()),
                                        &device,
                                    ).clicked() {
                                        selected_device = Some(device);
                                    }
                                }
                            });
                    });
                    if selected_device != cue.io.output_device {
                        if let Some(device_name) = selected_device.as_deref() {
                            audio.refresh_output_device_info(device_name);
                        }
                        cue.io.output_device = selected_device;
                        changed = true;
                    }
                    let output_channels = audio
                        .output_channels_for_device(cue.io.output_device.as_deref())
                        .max(1);
                    ui.label(RichText::new(format!("Available outputs for this track: {}", output_channels))
                        .color(Theme::TEXT_DIM).size(11.0));
                    let track_layouts = audio.output_channel_layouts_for_device(cue.io.output_device.as_deref());
                    if !track_layouts.is_empty() {
                        ui.label(RichText::new(format!("Driver layouts: {}", track_layouts.iter()
                            .map(|count| format!("{} ch", count)).collect::<Vec<_>>().join(", ")))
                            .color(Theme::TEXT_DIM).size(10.0));
                    }
                    ui.label(RichText::new("Device selection applies the next time this cue is started.")
                        .color(Theme::TEXT_DIM).size(10.0));
                    let input_channels = cue.audio_channels;
                    if input_channels == 0 {
                        ui.label(RichText::new("Could not detect this track's audio channel format")
                            .color(Theme::TEXT_DIM));
                        return;
                    }
                    cue.io.ensure_channel_routes(input_channels, output_channels);
                    let format = match input_channels {
                        1 => "Mono",
                        2 => "Stereo",
                        6 => "6-channel / 5.1 typical",
                        8 => "8-channel / 7.1 typical",
                        _ => "Multichannel",
                    };
                    ui.label(RichText::new(format!("{} · {} source channels", format, input_channels))
                        .color(Theme::ACCENT));
                    ui.add_space(6.0);

                    egui::ScrollArea::vertical()
                        .id_salt(("io_track_routes_scroll", cue_id))
                        .max_height(300.0)
                        .show(ui, |ui| {
                        for (source_index, route) in cue.io.channel_routes.iter_mut().enumerate() {
                            ui.horizontal(|ui| {
                                changed |= ui.checkbox(&mut route.active, "Active").changed();
                                let source_name = match (input_channels, source_index) {
                                    (1, 0) => "Mono (1)".to_string(),
                                    (2, 0) => "Left (1)".to_string(),
                                    (2, 1) => "Right (2)".to_string(),
                                    (_, index) => format!("Channel {}", index + 1),
                                };
                                ui.add_sized([92.0, 22.0], Label::new(source_name));

                                egui::ComboBox::from_id_salt(("cue_output_route", cue_id, source_index))
                                    .width(105.0)
                                    .selected_text(format!("Output {}", route.output_channel + 1))
                                    .show_ui(ui, |ui| {
                                        for output_index in 0..output_channels {
                                            changed |= ui.selectable_value(
                                                &mut route.output_channel,
                                                output_index,
                                                format!("Output {}", output_index + 1),
                                            ).changed();
                                        }
                                    });

                                let slider_changed = ui.add(
                                    Slider::new(&mut route.gain_db, -60.0..=18.0)
                                        .show_value(false),
                                ).changed();
                                if slider_changed {
                                    ui.ctx().data_mut(|data| data.insert_temp(
                                        Id::new(("track_output_gain_db", cue_id, source_index)).with("gain_db_text"),
                                        format!("{:.1}", route.gain_db),
                                    ));
                                }
                                changed |= slider_changed;
                                changed |= Self::gain_db_input(
                                    ui,
                                    Id::new(("track_output_gain_db", cue_id, source_index)),
                                    &mut route.gain_db,
                                );
                                ui.label("dB");
                            });
                        }
                    });

                    if let Some(left) = cue.io.channel_routes.first() {
                        cue.io.left_enabled = left.active;
                        cue.io.left_gain_db = left.gain_db;
                    }
                    if let Some(right) = cue.io.channel_routes.get(1) {
                        cue.io.right_enabled = right.active;
                        cue.io.right_gain_db = right.gain_db;
                    }
                } else {
                    ui.label(RichText::new("Select an audio cue to configure its source channels")
                        .color(Theme::TEXT_DIM));
                }
            });
        });

        if changed {
            audio.set_output_channel_settings(output_settings);
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

    fn fade_curve_preview(ui: &mut Ui, curve: FadeCurve, fade_in: bool) {
        let (rect, _) = ui.allocate_exact_size(vec2(300.0, 72.0), Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 3.0, Theme::BG_DARK);
        painter.rect_stroke(rect, 3.0, Stroke::new(1.0_f32, Theme::BORDER), StrokeKind::Inside);
        let points: Vec<Pos2> = (0..=48)
            .map(|index| {
                let t = index as f32 / 48.0;
                let progress = curve.apply(t);
                let level = if fade_in { progress } else { 1.0 - progress };
                pos2(rect.left() + t * rect.width(), rect.bottom() - level * rect.height())
            })
            .collect();
        painter.add(Shape::line(points, Stroke::new(2.0_f32, Theme::ACCENT)));
    }

    fn custom_fade_editor(ui: &mut Ui, points: &mut Vec<[f32; 2]>, fade_in: bool) {
        ui.add_space(4.0);
        ui.label(
            RichText::new("Draw the fade curve by dragging inside the graph")
                .color(Theme::TEXT_DIM)
                .size(11.0),
        );

        let (rect, response) = ui.allocate_exact_size(vec2(300.0, 118.0), Sense::drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 3.0, Theme::BG_DARK);
        painter.rect_stroke(
            rect,
            3.0,
            Stroke::new(1.0_f32, Theme::BORDER),
            StrokeKind::Inside,
        );

        for step in 1..4 {
            let t = step as f32 / 4.0;
            let x = rect.left() + rect.width() * t;
            let y = rect.bottom() - rect.height() * t;
            painter.line_segment(
                [pos2(x, rect.top()), pos2(x, rect.bottom())],
                Stroke::new(1.0_f32, Theme::BORDER),
            );
            painter.line_segment(
                [pos2(rect.left(), y), pos2(rect.right(), y)],
                Stroke::new(1.0_f32, Theme::BORDER),
            );
        }

        if response.dragged() {
            if let Some(pointer) = response.interact_pointer_pos() {
                let x = ((pointer.x - rect.left()) / rect.width()).clamp(0.01, 0.99);
                let displayed_level = (1.0 - (pointer.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
                let y = if fade_in { displayed_level } else { 1.0 - displayed_level };
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
                    rect.bottom() - (if fade_in { point[1] } else { 1.0 - point[1] })
                        .clamp(0.0, 1.0) * rect.height(),
                )
            })
            .collect();
        if curve_points.len() >= 2 {
            painter.add(Shape::line(curve_points, Stroke::new(2.0_f32, Theme::ACCENT)));
        }

        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{} control points", points.len()))
                    .color(Theme::TEXT_DIM)
                    .size(10.0),
            );
            if ui.small_button("Reset linear").clicked() {
                *points = vec![[0.0, 0.0], [1.0, 1.0]];
            }
        });
    }

    fn section(ui: &mut Ui, title: &str, content: impl FnOnce(&mut Ui)) {
        ui.label(RichText::new(title).color(Theme::ACCENT).size(11.5).strong());
        ui.indent(title, |ui| {
            content(ui);
        });
    }

    fn gain_db_input(ui: &mut Ui, id: Id, value: &mut f32) -> bool {
        let text_id = id.with("gain_db_text");
        let mut text = ui.ctx().data(|data| data.get_temp::<String>(text_id))
            .unwrap_or_else(|| format!("{:.1}", *value));
        let response = ui.add_sized(
            [66.0, 22.0],
            TextEdit::singleline(&mut text).desired_width(66.0),
        );
        let mut changed = false;
        if response.double_clicked() {
            *value = 0.0;
            text = "0.0".to_string();
            changed = true;
        } else if response.lost_focus()
            || (response.has_focus() && ui.input(|input| input.key_pressed(Key::Enter)))
        {
            let parsed = if text.trim() == "-" {
                Some(-60.0)
            } else {
                text.trim().parse::<f32>().ok()
            };
            if let Some(parsed) = parsed {
                let next = parsed.clamp(-60.0, 18.0);
                changed |= (*value - next).abs() > f32::EPSILON;
                *value = next;
            }
            text = format!("{:.1}", *value);
        }
        ui.ctx().data_mut(|data| data.insert_temp(text_id, text));
        changed
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
