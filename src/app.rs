// CueLabs — main application state, wires together all subsystems

use crate::audio::engine::{AudioCommand, AudioEngine};
use crate::audio::waveform::{generate_waveform, get_duration};
use crate::config::HotkeyConfig;
use crate::cue::{AudioCue, CueList, CueState};
use crate::export::session_report::SessionReport;
use crate::midi::input::MidiInputManager;
use crate::osc::server::OscServer;
use crate::project::{asset_manager::AssetManager, save_load::Project};
use crate::ui::{
    cue_table::{CueTable, CueTableAction},
    inspector::Inspector,
    theme::Theme,
    transport::{TransportAction, TransportBar},
    waveform_editor::WaveformEditor,
};

use eframe::egui::{self, Context, Key};
use rfd::FileDialog;
use std::path::PathBuf;
use tracing::{error, info, warn};
use uuid::Uuid;

// ────────────────────────────────────────────────────────────────────────────
// Side-panel tabs
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Panel {
    Inspector,
    Waveform,
    Report,
    Settings,
}

// ────────────────────────────────────────────────────────────────────────────
// Application state
// ────────────────────────────────────────────────────────────────────────────

pub struct CueLabsApp {
    // Subsystems
    audio:   AudioEngine,
    midi:    MidiInputManager,
    osc:     Option<OscServer>,

    // Data
    cue_list:     CueList,
    project:      Project,
    project_path: Option<PathBuf>,  // path to saved .clabs file
    assets:       AssetManager,
    report:       SessionReport,
    hotkeys:      HotkeyConfig,

    // UI state
    selected:     Option<Uuid>,
    renaming:     Option<(Uuid, String)>,
    active_panel: Panel,
    master_vol:   f32,
    status_msg:   String,
}

impl CueLabsApp {
    pub fn new(cc: &eframe::CreationContext) -> Self {
        Theme::apply(&cc.egui_ctx);

        let audio = AudioEngine::new().unwrap_or_else(|e| {
            panic!("Cannot initialise audio engine: {e}");
        });

        let midi = MidiInputManager::new();
        if midi.port_names.is_empty() {
            warn!("No MIDI ports found");
        } else {
            info!("MIDI ports: {:?}", midi.port_names);
        }

        let osc = OscServer::new(7700, "127.0.0.1:7701".to_string())
            .map_err(|e| warn!("OSC unavailable: {e}"))
            .ok();

        Self {
            audio,
            midi,
            osc,
            cue_list:     CueList::new("Main"),
            project:      Project::default(),
            project_path: None,
            assets:       AssetManager::new(),
            report:       SessionReport::new("Untitled Project"),
            hotkeys:      HotkeyConfig::default(),
            selected:     None,
            renaming:     None,
            active_panel: Panel::Inspector,
            master_vol:   1.0,
            status_msg:   "Ready — drag audio files here or press Ctrl+I".to_string(),
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn selected_cue_id(&self) -> Option<Uuid> {
        self.selected
    }

    fn cue_by_id_mut(&mut self, id: Uuid) -> Option<&mut AudioCue> {
        self.cue_list.find_mut(id)
    }

    // ── Transport actions ────────────────────────────────────────────────────

    fn go(&mut self) {
        let id = match self.selected { Some(id) => id, None => return };
        if let Some(cue) = self.cue_list.find_mut(id) {
            cue.state = CueState::Playing;
            self.audio.send(AudioCommand::Play {
                id: cue.id,
                file: cue.file_path.clone(),
                start_secs: cue.start_secs,
                volume: cue.volume * self.master_vol,
                fade_in_secs: cue.fade_in_secs,
                loop_enabled: cue.loop_region.enabled,
                loop_start: cue.loop_region.start_secs,
                loop_end: cue.loop_region.end_secs,
                loop_count: cue.loop_region.count,
            });
            self.report.record(&cue.number.clone(), &cue.name.clone(), 0.0, "Play");
            self.status_msg = format!("▶ Playing: {}", cue.name);
        }
    }

    fn stop_selected(&mut self) {
        let id = match self.selected { Some(id) => id, None => return };
        if let Some(cue) = self.cue_list.find_mut(id) {
            self.report.record(&cue.number.clone(), &cue.name.clone(), cue.position_secs, "Stop");
            cue.state = CueState::Stopped;
            cue.position_secs = 0.0;
        }
        self.audio.send(AudioCommand::Stop(id));
        self.status_msg = "⏹ Stopped".to_string();
    }

    fn stop_all(&mut self) {
        for cue in &mut self.cue_list.cues {
            cue.state = CueState::Stopped;
            cue.position_secs = 0.0;
        }
        self.audio.send(AudioCommand::StopAll);
        self.status_msg = "⏹ All stopped".to_string();
    }

    fn pause_selected(&mut self) {
        let id = match self.selected { Some(id) => id, None => return };
        if let Some(cue) = self.cue_list.find_mut(id) {
            cue.state = CueState::Paused;
            self.audio.send(AudioCommand::Pause(id));
            self.report.record(&cue.number.clone(), &cue.name.clone(), cue.position_secs, "Pause");
        }
    }

    fn pause_all(&mut self) {
        self.cue_list.pause_all();
        self.audio.send(AudioCommand::PauseAll);
        self.status_msg = "⏸ All paused".to_string();
    }

    fn resume_selected(&mut self) {
        let id = match self.selected { Some(id) => id, None => return };
        if let Some(cue) = self.cue_list.find_mut(id) {
            cue.state = CueState::Playing;
            self.audio.send(AudioCommand::Resume(id));
            self.report.record(&cue.number.clone(), &cue.name.clone(), cue.position_secs, "Resume");
        }
    }

    fn resume_all(&mut self) {
        self.cue_list.resume_all();
        self.audio.send(AudioCommand::ResumeAll);
        self.status_msg = "▶ All resumed".to_string();
    }

    // ── Cue management ───────────────────────────────────────────────────────

    fn add_files(&mut self) {
        let files = FileDialog::new()
            .add_filter("Audio", &["wav", "mp3", "flac", "ogg", "aiff", "aif", "m4a"])
            .pick_files();

        if let Some(paths) = files {
            for path in paths {
                self.import_file(path);
            }
        }
    }

    fn import_file(&mut self, path: PathBuf) {
        // Try to copy into project assets (only if project is saved)
        let final_path = if self.project_path.is_some() {
            self.assets.import_file(&path).unwrap_or_else(|_| path.clone())
        } else {
            path.clone()
        };

        let mut cue = AudioCue::new(final_path.clone());

        // Load duration + waveform (blocking, fine for short files; Phase 2: thread)
        let dur = get_duration(&final_path);
        if dur > 0.0 {
            cue.duration_secs = dur;
        }
        cue.waveform = generate_waveform(&final_path, 512);

        let name = cue.name.clone();
        self.cue_list.add(cue);
        self.selected = self.cue_list.cues.last().map(|c| c.id);
        self.status_msg = format!("Added: {name}");
    }

    fn delete_cue(&mut self, id: Uuid) {
        self.audio.send(AudioCommand::Stop(id));
        if self.selected == Some(id) { self.selected = None; }
        self.cue_list.remove(id);
        self.cue_list.renumber();
    }

    fn select_next(&mut self) {
        if self.cue_list.cues.is_empty() { return; }
        self.selected = Some(match self.selected {
            None => self.cue_list.cues[0].id,
            Some(cur) => {
                let pos = self.cue_list.cues.iter().position(|c| c.id == cur).unwrap_or(0);
                self.cue_list.cues[(pos + 1).min(self.cue_list.cues.len() - 1)].id
            }
        });
    }

    fn select_prev(&mut self) {
        if self.cue_list.cues.is_empty() { return; }
        self.selected = Some(match self.selected {
            None => self.cue_list.cues[0].id,
            Some(cur) => {
                let pos = self.cue_list.cues.iter().position(|c| c.id == cur).unwrap_or(0);
                self.cue_list.cues[pos.saturating_sub(1)].id
            }
        });
    }

    // ── Project I/O ──────────────────────────────────────────────────────────

    fn save_project(&mut self) {
        // Pick save path on first save
        if self.project_path.is_none() {
            let path = FileDialog::new()
                .add_filter("CueLabs Project", &["clabs"])
                .save_file();
            match path {
                Some(p) => {
                    let p = if p.extension().is_none() { p.with_extension("clabs") } else { p };
                    self.assets.set_project_path(p.clone());
                    self.project_path = Some(p);
                }
                None => return,
            }
        }

        let path = self.project_path.clone().unwrap();
        self.project.cue_list = self.cue_list.clone();

        match self.project.save(&path) {
            Ok(_) => self.status_msg = format!("💾 Saved: {}", path.display()),
            Err(e) => { self.status_msg = format!("Save error: {e}"); error!("{e}"); }
        }
    }

    fn open_project(&mut self) {
        self.stop_all();

        let file = FileDialog::new()
            .add_filter("CueLabs Project", &["clabs"])
            .pick_file();

        if let Some(path) = file {
            match Project::load(&path) {
                Ok(proj) => {
                    // Reload waveforms (they're not serialised)
                    let mut cl = proj.cue_list.clone();
                    for cue in &mut cl.cues {
                        if cue.duration_secs == 0.0 {
                            let d = get_duration(&cue.file_path);
                        if d > 0.0 {
                            cue.duration_secs = d;
                        }
                        }
                        if cue.waveform.is_empty() {
                            cue.waveform = generate_waveform(&cue.file_path, 512);
                        }
                    }

                    self.cue_list = cl;
                    self.assets.set_project_path(path.clone());
                    self.project_path = Some(path.clone());
                    self.report = SessionReport::new(&proj.settings.name);
                    self.project = proj;
                    self.selected = None;
                    self.status_msg = format!("📂 Opened: {}", path.display());
                }
                Err(e) => { self.status_msg = format!("Open error: {e}"); error!("{e}"); }
            }
        }
    }

    fn new_project(&mut self) {
        self.stop_all();
        self.cue_list   = CueList::new("Main");
        self.project     = Project::default();
        self.project_path = None;
        self.report      = SessionReport::new("Untitled Project");
        self.selected    = None;
        self.status_msg  = "New project".to_string();
    }

    // ── Position polling ─────────────────────────────────────────────────────

    fn poll_audio(&mut self) {
        for upd in self.audio.drain_position_updates() {
            if let Some(cue) = self.cue_list.find_mut(upd.id) {
                cue.state        = upd.state.clone();
                cue.position_secs = upd.position_secs;
                if upd.state == CueState::Stopped { cue.position_secs = 0.0; }
            }
        }
    }

    fn poll_midi(&mut self) {
        for msg in self.midi.drain() {
            use crate::midi::input::MidiMessage;
            match msg {
                MidiMessage::NoteOn { channel: _, note, velocity: _ } => {
                    // Find cue with matching MIDI trigger
                    if let Some(cue_id) = self.cue_list.cues.iter()
                        .find(|c| {
                            matches!(&c.midi_trigger,
                                Some(crate::cue::MidiTrigger::NoteOn { note: n, .. }) if *n == note)
                        })
                        .map(|c| c.id)
                    {
                        self.selected = Some(cue_id);
                        self.go();
                    }
                }
                MidiMessage::CC { channel: _, cc, value } => {
                    // Master volume via CC 7
                    if cc == 7 {
                        let vol = value as f32 / 127.0;
                        self.master_vol = vol;
                        for cue in self.cue_list.cues.iter().filter(|c| c.is_active()) {
                            self.audio.send(AudioCommand::SetVolume(cue.id, cue.volume * vol));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // ── Keyboard input ───────────────────────────────────────────────────────

    fn handle_keyboard(&mut self, ctx: &Context) {
        // Don't steal keys when a text field has focus
        if ctx.memory(|m| m.focused().is_some()) { return; }

        ctx.input(|inp| {
            let ctrl  = inp.modifiers.ctrl;
            let shift = inp.modifiers.shift;
            let alt   = inp.modifiers.alt;
            let bare  = !ctrl && !shift && !alt;

            if bare {
                if inp.key_pressed(Key::Space)     { return; }  // handled below (outside closure)
                if inp.key_pressed(Key::S)         { return; }
                if inp.key_pressed(Key::Escape)    { return; }
                if inp.key_pressed(Key::P)         { return; }
                if inp.key_pressed(Key::R)         { return; }
                if inp.key_pressed(Key::ArrowDown) { return; }
                if inp.key_pressed(Key::ArrowUp)   { return; }
                if inp.key_pressed(Key::Delete)    { return; }
            }
        });

        // Use immutable borrow for key detection, then act
        let keys: Vec<(Key, bool, bool, bool)> = ctx.input(|inp| {
            let mut v = Vec::new();
            for &k in &[
                Key::Space, Key::S, Key::Escape, Key::P, Key::R,
                Key::ArrowDown, Key::ArrowUp, Key::Delete,
                Key::N, Key::O,
            ] {
                if inp.key_pressed(k) {
                    v.push((k, inp.modifiers.ctrl, inp.modifiers.shift, inp.modifiers.alt));
                }
            }
            v
        });

        for (key, ctrl, shift, alt) in keys {
            let bare = !ctrl && !shift && !alt;
            match (key, ctrl, shift, alt) {
                (Key::Space,     false, false, false) => self.go(),
                (Key::S,         false, false, false) => self.stop_selected(),
                (Key::Escape,    false, false, false) => self.stop_all(),
                (Key::P,         false, false, false) => self.pause_selected(),
                (Key::P,         false, true,  false) => self.pause_all(),
                (Key::R,         false, false, false) => self.resume_selected(),
                (Key::R,         false, true,  false) => self.resume_all(),
                (Key::ArrowDown, false, false, false) => self.select_next(),
                (Key::ArrowUp,   false, false, false) => self.select_prev(),
                (Key::Delete,    false, false, false) => {
                    if let Some(id) = self.selected { self.delete_cue(id); }
                }
                (Key::S,  true,  false, false) => self.save_project(),
                (Key::O,  true,  false, false) => self.open_project(),
                (Key::N,  true,  false, false) => self.new_project(),
                _ => {}
            }
        }
    }

    // ── Drag-and-drop ────────────────────────────────────────────────────────

    fn handle_drop(&mut self, ctx: &Context) {
        let audio_exts = ["wav", "mp3", "flac", "ogg", "aiff", "aif", "m4a"];
        ctx.input(|inp| {
            inp.raw.dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .filter(|p| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| audio_exts.contains(&e.to_lowercase().as_str()))
                        .unwrap_or(false)
                })
                .collect::<Vec<_>>()
        })
        .into_iter()
        .for_each(|p| self.import_file(p));
    }

    // ── Menu bar ─────────────────────────────────────────────────────────────

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::menu::bar(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("New project    Ctrl+N").clicked() { self.new_project(); ui.close_menu(); }
                if ui.button("Open…          Ctrl+O").clicked() { self.open_project(); ui.close_menu(); }
                if ui.button("Save           Ctrl+S").clicked() { self.save_project(); ui.close_menu(); }
                ui.separator();
                if ui.button("Add files…     Ctrl+I").clicked() { self.add_files(); ui.close_menu(); }
                ui.separator();
                if ui.button("Export report…").clicked() {
                    if let Some(dir) = FileDialog::new().pick_folder() {
                        match self.report.save(&dir) {
                            Ok(p) => self.status_msg = format!("Report saved: {}", p.display()),
                            Err(e) => self.status_msg = format!("Report error: {e}"),
                        }
                    }
                    ui.close_menu();
                }
            });

            ui.menu_button("View", |ui| {
                if ui.button("Inspector").clicked()      { self.active_panel = Panel::Inspector; ui.close_menu(); }
                if ui.button("Waveform Editor").clicked() { self.active_panel = Panel::Waveform; ui.close_menu(); }
                if ui.button("Session Report").clicked() { self.active_panel = Panel::Report; ui.close_menu(); }
                if ui.button("Settings").clicked()       { self.active_panel = Panel::Settings; ui.close_menu(); }
            });

            ui.menu_button("Transport", |ui| {
                if ui.button("GO          Space").clicked()   { self.go(); ui.close_menu(); }
                if ui.button("Stop        S").clicked()       { self.stop_selected(); ui.close_menu(); }
                if ui.button("Stop All    Esc").clicked()     { self.stop_all(); ui.close_menu(); }
                if ui.button("Pause       P").clicked()       { self.pause_selected(); ui.close_menu(); }
                if ui.button("Pause All   Shift+P").clicked() { self.pause_all(); ui.close_menu(); }
                if ui.button("Resume      R").clicked()       { self.resume_selected(); ui.close_menu(); }
                if ui.button("Resume All  Shift+R").clicked() { self.resume_all(); ui.close_menu(); }
            });
        });
    }

    // ── Right panel ──────────────────────────────────────────────────────────

    fn right_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.active_panel, Panel::Inspector, "Inspector");
            ui.selectable_value(&mut self.active_panel, Panel::Waveform,  "Waveform");
            ui.selectable_value(&mut self.active_panel, Panel::Report,    "Report");
            ui.selectable_value(&mut self.active_panel, Panel::Settings,  "Settings");
        });
        ui.separator();

        match self.active_panel {
            Panel::Inspector => {
                if let Some(id) = self.selected {
                    if let Some(cue) = self.cue_list.find_mut(id) {
                        Inspector::show(ui, cue);
                    }
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new("Select a cue to inspect")
                                .color(Theme::TEXT_DIM)
                        );
                    });
                }
            }

            Panel::Waveform => {
                if let Some(id) = self.selected {
                    if let Some(cue) = self.cue_list.find_mut(id) {
                        WaveformEditor::show(ui, cue);
                    }
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new("Select a cue to view waveform")
                                .color(Theme::TEXT_DIM)
                        );
                    });
                }
            }

            Panel::Report => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let text = self.report.to_text_summary();
                    ui.label(
                        egui::RichText::new(text).monospace().size(11.0).color(Theme::TEXT_DIM)
                    );
                });
            }

            Panel::Settings => self.settings_panel(ui),
        }
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Project");
            ui.horizontal(|ui| {
                ui.label("Name:");
                ui.text_edit_singleline(&mut self.project.settings.name);
            });
            ui.horizontal(|ui| {
                ui.label("Author:");
                ui.text_edit_singleline(&mut self.project.settings.author);
            });

            ui.add_space(8.0);
            ui.heading("Audio");
            ui.label(format!("Device: {}", self.audio.current_device));
            ui.horizontal(|ui| {
                ui.label("Auto crossfade:");
                ui.checkbox(&mut self.project.settings.auto_crossfade, "");
            });
            ui.horizontal(|ui| {
                ui.label("Crossfade (s):");
                ui.add(egui::DragValue::new(&mut self.project.settings.crossfade_secs)
                    .range(0.1..=10.0).speed(0.1));
            });

            ui.add_space(8.0);
            ui.heading("OSC");
            ui.horizontal(|ui| {
                ui.label("Listen port:");
                ui.add(egui::DragValue::new(&mut self.project.settings.osc_listen_port));
            });
            ui.horizontal(|ui| {
                ui.label("Send to:");
                ui.text_edit_singleline(&mut self.project.settings.osc_send_addr);
            });

            ui.add_space(8.0);
            ui.heading("MIDI");
            if self.midi.port_names.is_empty() {
                ui.label(egui::RichText::new("No MIDI ports found").color(Theme::TEXT_DIM));
            } else {
                for p in &self.midi.port_names.clone() {
                    ui.label(format!("• {p}"));
                }
            }

            ui.add_space(8.0);
            ui.heading("Hotkeys");
            for (action, key) in &self.hotkeys.bindings {
                ui.horizontal(|ui| {
                    ui.label(format!("{:<28}", action.label()));
                    ui.monospace(key);
                });
            }
        });
    }
}

// ────────────────────────────────────────────────────────────────────────────
// eframe::App
// ────────────────────────────────────────────────────────────────────────────

impl eframe::App for CueLabsApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.poll_audio();
        self.poll_midi();
        self.handle_keyboard(ctx);
        self.handle_drop(ctx);

        // Continuous repaint while something is playing
        if self.cue_list.active_count() > 0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        // ── Menu bar ──────────────────────────────────────────────────────
        egui::TopBottomPanel::top("menu")
            .frame(egui::Frame::NONE
                .fill(Theme::BG_PANEL)
                .inner_margin(egui::Margin::symmetric(4_i8, 2_i8)))
            .show(ctx, |ui| { self.menu_bar(ui); });

        // ── Status bar ────────────────────────────────────────────────────
        egui::TopBottomPanel::bottom("status")
            .frame(egui::Frame::NONE
                .fill(Theme::BG_DARK)
                .inner_margin(egui::Margin::symmetric(8_i8, 2_i8)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&self.status_msg)
                            .color(Theme::TEXT_DIM).size(11.0),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(format!("🔊 {}", self.audio.current_device))
                                .color(Theme::TEXT_DIM).size(11.0),
                        );
                    });
                });
            });

        // ── Transport bar ─────────────────────────────────────────────────
        egui::TopBottomPanel::bottom("transport")
            .frame(egui::Frame::NONE
                .fill(Theme::BG_PANEL)
                .inner_margin(egui::Margin::symmetric(8_i8, 6_i8)))
            .show(ctx, |ui| {
                let active = self.cue_list.active_count();
                let action = TransportBar::show(ui, active, &mut self.master_vol);
                match action {
                    Some(TransportAction::Go)           => self.go(),
                    Some(TransportAction::StopSelected) => self.stop_selected(),
                    Some(TransportAction::StopAll)      => self.stop_all(),
                    Some(TransportAction::PauseSelected) => self.pause_selected(),
                    Some(TransportAction::PauseAll)     => self.pause_all(),
                    Some(TransportAction::ResumeSelected) => self.resume_selected(),
                    Some(TransportAction::ResumeAll)    => self.resume_all(),
                    None => {}
                }
            });

        // ── Right panel ───────────────────────────────────────────────────
        egui::SidePanel::right("right")
            .resizable(true)
            .min_width(320.0)
            .default_width(400.0)
            .frame(egui::Frame::NONE
                .fill(Theme::BG_PANEL)
                .inner_margin(egui::Margin::same(8_i8)))
            .show(ctx, |ui| { self.right_panel(ui); });

        // ── Central cue list ──────────────────────────────────────────────
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE
                .fill(Theme::BG_DARK)
                .inner_margin(egui::Margin::same(0_i8)))
            .show(ctx, |ui| {
                // Title + add button
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    let title = if self.project.settings.name.is_empty() {
                        "Untitled Project"
                    } else {
                        &self.project.settings.name
                    };
                    ui.heading(
                        egui::RichText::new(title).color(Theme::TEXT).size(14.0),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("  + Add Files  ").clicked() {
                            self.add_files();
                        }
                    });
                });
                ui.add(egui::Separator::default().horizontal());

                // Cue table — returns optional action
                let action = CueTable::show(
                    ui,
                    &mut self.cue_list.cues,
                    &mut self.selected,
                    &mut self.renaming,
                );

                if let Some(action) = action {
                    match action {
                        CueTableAction::Play(id) => {
                            self.selected = Some(id);
                            self.go();
                        }
                        CueTableAction::Stop(id) => {
                            self.selected = Some(id);
                            self.stop_selected();
                        }
                        CueTableAction::Pause(id) => {
                            self.selected = Some(id);
                            self.pause_selected();
                        }
                        CueTableAction::Delete(id) => {
                            self.delete_cue(id);
                        }
                        CueTableAction::StartRename(id, name) => {
                            self.renaming = Some((id, name));
                        }
                        CueTableAction::RenameConfirm(id, new_name) => {
                            if let Some(cue) = self.cue_list.find_mut(id) {
                                cue.name = new_name;
                            }
                            self.renaming = None;
                        }
                        CueTableAction::RenameCancel => {
                            self.renaming = None;
                        }
                        CueTableAction::SetColor(id, color) => {
                            if let Some(cue) = self.cue_list.find_mut(id) {
                                cue.color = color;
                            }
                        }
                    }
                }
            });
    }
}
