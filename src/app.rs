// src/app.rs — главное состояние приложения CueLabs

use crate::audio::engine::{AudioCommand, AudioEngine};
use crate::audio::waveform::{generate_waveform, get_duration};
use crate::config::HotkeyConfig;
use crate::cue::{ContinueMode, Cue, CueList, CueState, CueType, GroupMode, MidiMessageKind};
use crate::export::session_report::SessionReport;
use crate::midi::input::{MidiInputManager, MidiOutputManager};
use crate::osc::server::OscServer;
use crate::project::{asset_manager::AssetManager, save_load::Project};
use crate::ui::{
    cue_table::{CueTable, CueTableAction, EditField},
    inspector::Inspector,
    theme::Theme,
    transport::{TransportAction, TransportBar},
    waveform_editor::WaveformEditor,
};

use eframe::egui::{self, Context, Key};
use rfd::FileDialog;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;
use tracing::{error, warn};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
enum Panel {
    Inspector,
    Waveform,
    Playback,
    IoRouting,
    Midi,
    Report,
    Settings,
}

#[derive(Debug, Clone)]
struct PendingTrigger {
    cue_id: Uuid,
    fire_at: Instant,
    is_pre_wait: bool,
}

#[derive(Debug, Clone)]
struct ActivePlaylist {
    group_id: Uuid,
    children: Vec<Uuid>,
    current_idx: usize,
    fade_in_secs: f64,
    fade_out_secs: f64,
}

pub struct CueLabsApp {
    renaming_project: bool,
    project_name_buf: String,
    audio: AudioEngine,
    midi: MidiInputManager,
    midi_out: MidiOutputManager,
    osc: Option<OscServer>,

    cue_list: CueList,
    project: Project,
    project_path: Option<PathBuf>,
    assets: AssetManager,
    report: SessionReport,
    hotkeys: HotkeyConfig,

    selected: Option<Uuid>,
    editing: Option<(Uuid, EditField, String)>,
    expanded_groups: HashSet<Uuid>,
    active_panel: Panel,
    master_vol: f32,
    status_msg: String,
    //drag_hold_target: Option<(Uuid, Instant)>,
    pending_triggers: Vec<PendingTrigger>,
    active_playlists: Vec<ActivePlaylist>,
}

impl CueLabsApp {
    pub fn new(cc: &eframe::CreationContext) -> Self {
        Theme::apply(&cc.egui_ctx);

        let audio = AudioEngine::new().unwrap_or_else(|e| {
            panic!("Cannot initialise audio engine: {e}");
        });

        let midi = MidiInputManager::new();
        let midi_out = MidiOutputManager::new();

        let osc = OscServer::new(7700, "127.0.0.1:7701".to_string())
            .map_err(|e| warn!("OSC unavailable: {e}"))
            .ok();

        Self {
            audio,
            midi,
            midi_out,
            osc,
            cue_list: CueList::new("Main"),
            project: Project::default(),
            project_path: None,
            assets: AssetManager::new(),
            report: SessionReport::new("Untitled Project"),
            hotkeys: HotkeyConfig::default(),
            selected: None,
            editing: None,
            expanded_groups: HashSet::new(),
            active_panel: Panel::Inspector,
            master_vol: 1.0,
            status_msg: "Ready — drag audio files here or add events from the toolbar".to_string(),
            pending_triggers: Vec::new(),
            active_playlists: Vec::new(),
            renaming_project: false,
            project_name_buf: String::new(),
            //drag_hold_target: None,
        }
    }

    // ── Запуск и диспетчеризация всех типов событий ──────────────────────────

    fn go(&mut self) {
        let id = match self.selected {
            Some(id) => id,
            None => {
                if let Some(first) = self.cue_list.cues.first() {
                    first.id
                } else {
                    return;
                }
            }
        };
        self.trigger_cue(id);
        self.select_next();
    }

    fn trigger_cue(&mut self, id: Uuid) {
        let Some(cue) = self.cue_list.find_mut(id) else {
            return;
        };

        let pre_wait = cue.pre_wait_secs;
        let post_wait = cue.post_wait_secs;
        let continue_mode = cue.continue_mode.clone();

        if pre_wait > 0.0 {
            cue.state = CueState::Waiting;
            self.pending_triggers.push(PendingTrigger {
                cue_id: id,
                fire_at: Instant::now() + std::time::Duration::from_secs_f64(pre_wait),
                is_pre_wait: true,
            });
            self.status_msg = format!("⏳ Pre-wait ({:.1}s): {}", pre_wait, cue.name);
        } else {
            self.execute_cue_now(id);
        }

        if continue_mode == ContinueMode::AutoContinue {
            if let Some(next_id) = self.next_cue_id(id) {
                let delay = pre_wait + post_wait;
                if delay > 0.0 {
                    self.pending_triggers.push(PendingTrigger {
                        cue_id: next_id,
                        fire_at: Instant::now() + std::time::Duration::from_secs_f64(delay),
                        is_pre_wait: false,
                    });
                } else {
                    self.trigger_cue(next_id);
                }
            }
        }
    }

    fn execute_cue_now(&mut self, id: Uuid) {
        let Some(cue_snapshot) = self.cue_list.find(id).cloned() else {
            return;
        };

        match cue_snapshot.cue_type {
            CueType::Audio => {
                if let Some(c) = self.cue_list.find_mut(id) {
                    c.state = CueState::Playing;
                }
                self.audio.send(AudioCommand::Play {
                    id: cue_snapshot.id,
                    file: cue_snapshot.file_path.clone(),
                    start_secs: cue_snapshot.start_secs,
                    end_secs: cue_snapshot.end_secs,
                    duration_secs: cue_snapshot.duration_secs,
                    volume: cue_snapshot.volume * self.master_vol,
                    fade_in_secs: cue_snapshot.fade_in_secs,
                    fade_out_secs: cue_snapshot.fade_out_secs,
                    fade_curve: cue_snapshot.fade_curve,
                    loop_enabled: cue_snapshot.loop_region.enabled,
                    loop_start: cue_snapshot.loop_region.start_secs,
                    loop_end: cue_snapshot.loop_region.end_secs,
                    loop_count: cue_snapshot.loop_region.count,
                });
                self.report
                    .record(&cue_snapshot.number, &cue_snapshot.name, 0.0, "Play");
                self.status_msg = format!("▶ Playing: {}", cue_snapshot.name);
            }

            CueType::Play { target_id } => {
                if let Some(tid) = target_id {
                    self.execute_cue_now(tid);
                } else if !cue_snapshot.file_path.as_os_str().is_empty() {
                    self.audio.send(AudioCommand::Play {
                        id: cue_snapshot.id,
                        file: cue_snapshot.file_path.clone(),
                        start_secs: cue_snapshot.start_secs,
                        end_secs: cue_snapshot.end_secs,
                        duration_secs: cue_snapshot.duration_secs,
                        volume: cue_snapshot.volume * self.master_vol,
                        fade_in_secs: cue_snapshot.fade_in_secs,
                        fade_out_secs: cue_snapshot.fade_out_secs,
                        fade_curve: cue_snapshot.fade_curve,
                        loop_enabled: cue_snapshot.loop_region.enabled,
                        loop_start: cue_snapshot.loop_region.start_secs,
                        loop_end: cue_snapshot.loop_region.end_secs,
                        loop_count: cue_snapshot.loop_region.count,
                    });
                }
                self.check_instant_auto_follow(id);
            }

            CueType::Pause { target_id } => {
                if let Some(tid) = target_id {
                    if let Some(tc) = self.cue_list.find_mut(tid) {
                        tc.state = CueState::Paused;
                    }
                    self.audio.send(AudioCommand::Pause(tid));
                } else {
                    self.pause_all();
                }
                self.check_instant_auto_follow(id);
            }

            CueType::Stop { target_id } => {
                if let Some(tid) = target_id {
                    if let Some(tc) = self.cue_list.find_mut(tid) {
                        tc.state = CueState::Stopped;
                        tc.position_secs = 0.0;
                    }
                    self.audio.send(AudioCommand::Stop(tid));
                } else {
                    self.stop_all();
                }
                self.check_instant_auto_follow(id);
            }

            CueType::Fade {
                target_id,
                fade_in,
                duration_secs,
                stop_on_finish,
            } => {
                if let Some(tid) = target_id {
                    let target_vol = if fade_in {
                        let base = self.cue_list.find(tid).map(|c| c.volume).unwrap_or(1.0);
                        base * self.master_vol
                    } else {
                        0.0
                    };
                    self.audio.send(AudioCommand::Fade {
                        id: tid,
                        target_volume: target_vol,
                        duration_secs,
                        stop_on_finish: !fade_in && stop_on_finish,
                    });
                    self.status_msg = format!("{} ({:.1}s)", cue_snapshot.name, duration_secs);
                }
                if cue_snapshot.continue_mode == ContinueMode::AutoFollow {
                    if let Some(next_id) = self.next_cue_id(id) {
                        self.pending_triggers.push(PendingTrigger {
                            cue_id: next_id,
                            fire_at: Instant::now()
                                + std::time::Duration::from_secs_f64(duration_secs),
                            is_pre_wait: false,
                        });
                    }
                }
            }

            CueType::Midi {
                channel,
                message,
                raw_hex,
            } => {
                let bytes =
                    if !raw_hex.trim().is_empty() && matches!(message, MidiMessageKind::Raw(_)) {
                        MidiMessageKind::parse_hex(&raw_hex)
                    } else {
                        message.to_bytes(channel)
                    };
                let _ = self.midi_out.send_bytes(&bytes);
                self.report
                    .record(&cue_snapshot.number, &cue_snapshot.name, 0.0, "MIDI");
                self.status_msg = format!("🎹 MIDI Sent: {:02X?}", bytes);
                self.check_instant_auto_follow(id);
            }

            CueType::Group {
                mode,
                children,
                fade_in_secs,
                fade_out_secs,
            } => {
                match mode {
                    GroupMode::Normal => {
                        // Разворачиваем группу, запускаем только первого ребёнка.
                        // Дальше цепочка идёт сама по auto-continue/auto-follow детей.
                        // Когда цепочка обрывается (ребёнок с Nothing) — он остаётся выделенным.
                        self.expanded_groups.insert(id);
                        if let Some(first_id) = children.first().copied() {
                            self.selected = Some(first_id);
                            self.trigger_cue(first_id);
                            self.status_msg = format!("📁 Group (Normal): starting child");
                        } else {
                            self.status_msg = "📁 Group is empty".to_string();
                        }
                    }
                    GroupMode::Playlist => {
                        // Последовательно проигрываем всех детей.
                        self.expanded_groups.insert(id);
                        if children.is_empty() {
                            return;
                        }
                        let group_id = cue_snapshot.id;
                        let kids = children.clone();
                        let fi = fade_in_secs;
                        let fo = fade_out_secs;
                        self.start_playlist_child(group_id, kids, 0, fi, fo);
                    }
                }
                self.check_instant_auto_follow(id);
            }
        }
    }

    fn check_instant_auto_follow(&mut self, id: Uuid) {
        if let Some(cue) = self.cue_list.find(id) {
            if cue.continue_mode == ContinueMode::AutoFollow {
                if let Some(next_id) = self.next_cue_id(id) {
                    self.trigger_cue(next_id);
                }
            }
        }
    }

    fn start_playlist_child(
        &mut self,
        group_id: Uuid,
        children: Vec<Uuid>,
        idx: usize,
        fade_in: f64,
        fade_out: f64,
    ) {
        let Some(&child_id) = children.get(idx) else {
            return;
        };

        // Сохраняем/обновляем прогресс плейлиста
        self.active_playlists.retain(|p| p.group_id != group_id);
        self.active_playlists.push(ActivePlaylist {
            group_id,
            children: children.clone(),
            current_idx: idx,
            fade_in_secs: fade_in,
            fade_out_secs: fade_out,
        });

        // Накатываем fade-настройки группы на ребёнка до запуска
        if let Some(child) = self.cue_list.find_mut(child_id) {
            if fade_in > 0.0 {
                child.fade_in_secs = fade_in;
            }
            if fade_out > 0.0 {
                child.fade_out_secs = fade_out;
            }
        }

        self.selected = Some(child_id);
        self.trigger_cue(child_id);
        self.status_msg = format!("📁 Playlist: child {}/{}", idx + 1, children.len());
    }

    fn handle_drag_drop(&mut self, dragged_id: Uuid, target_idx: usize) {
        // 1) Если цель — группа, кладём туда как в папку
        let is_group = self
            .cue_list
            .cues
            .get(target_idx)
            .map(|c| matches!(c.cue_type, CueType::Group { .. }))
            .unwrap_or(false);

        if is_group {
            let group_id = self.cue_list.cues[target_idx].id;
            if dragged_id == group_id {
                return;
            }

            // Если уже в какой-то группе — вычищаем из старой
            for c in &mut self.cue_list.cues {
                if let CueType::Group { children, .. } = &mut c.cue_type {
                    children.retain(|x| *x != dragged_id);
                }
            }

            // Добавляем в целевую
            if let Some(target) = self.cue_list.cues.get_mut(target_idx) {
                if let CueType::Group { children, .. } = &mut target.cue_type {
                    children.push(dragged_id);
                }
            }
            self.expanded_groups.insert(group_id);
            self.status_msg = "Added cue to group via drag-and-drop".to_string();
            return;
        }

        // 2) Иначе — обычная перестановка
        if let Some(from_idx) = self.cue_list.cues.iter().position(|c| c.id == dragged_id) {
            let item = self.cue_list.cues.remove(from_idx);
            let insert_idx = if target_idx > from_idx {
                target_idx - 1
            } else {
                target_idx
            };
            self.cue_list
                .cues
                .insert(insert_idx.min(self.cue_list.cues.len()), item);
            self.cue_list.renumber();
        }
    }

    fn next_cue_id(&self, current_id: Uuid) -> Option<Uuid> {
        let pos = self.cue_list.cues.iter().position(|c| c.id == current_id)?;
        self.cue_list.cues.get(pos + 1).map(|c| c.id)
    }

    fn poll_pending_triggers(&mut self) {
        let now = Instant::now();
        let mut ready = Vec::new();
        self.pending_triggers.retain(|t| {
            if now >= t.fire_at {
                ready.push(t.clone());
                false
            } else {
                true
            }
        });

        for trig in ready {
            if trig.is_pre_wait {
                self.execute_cue_now(trig.cue_id);
            } else {
                self.trigger_cue(trig.cue_id);
            }
        }
    }

    // ── Transport actions ────────────────────────────────────────────────────

    fn stop_selected(&mut self) {
        let id = match self.selected {
            Some(id) => id,
            None => return,
        };
        if let Some(cue) = self.cue_list.find_mut(id) {
            self.report.record(
                &cue.number.clone(),
                &cue.name.clone(),
                cue.position_secs,
                "Stop",
            );
            cue.state = CueState::Stopped;
            cue.position_secs = 0.0;
        }
        self.audio.send(AudioCommand::Stop(id));
        self.status_msg = "⏹ Stopped".to_string();
    }

    fn stop_all(&mut self) {
        self.pending_triggers.clear();
        self.cue_list.stop_all();
        self.audio.send(AudioCommand::StopAll);
        self.status_msg = "⏹ All stopped".to_string();
    }

    fn pause_selected(&mut self) {
        let id = match self.selected {
            Some(id) => id,
            None => return,
        };
        if let Some(cue) = self.cue_list.find_mut(id) {
            cue.state = CueState::Paused;
            self.audio.send(AudioCommand::Pause(id));
            self.report.record(
                &cue.number.clone(),
                &cue.name.clone(),
                cue.position_secs,
                "Pause",
            );
        }
    }

    fn pause_all(&mut self) {
        self.cue_list.pause_all();
        self.audio.send(AudioCommand::PauseAll);
        self.status_msg = "⏸ All paused".to_string();
    }

    fn resume_selected(&mut self) {
        let id = match self.selected {
            Some(id) => id,
            None => return,
        };
        if let Some(cue) = self.cue_list.find_mut(id) {
            cue.state = CueState::Playing;
            self.audio.send(AudioCommand::Resume(id));
            self.report.record(
                &cue.number.clone(),
                &cue.name.clone(),
                cue.position_secs,
                "Resume",
            );
        }
    }

    fn resume_all(&mut self) {
        self.cue_list.resume_all();
        self.audio.send(AudioCommand::ResumeAll);
        self.status_msg = "▶ All resumed".to_string();
    }

    // ── Создание и управление событиями ──────────────────────────────────────

    fn add_files(&mut self) {
        let files = FileDialog::new()
            .add_filter(
                "Audio",
                &["wav", "mp3", "flac", "ogg", "aiff", "aif", "m4a"],
            )
            .pick_files();

        if let Some(paths) = files {
            for path in paths {
                self.import_file(path);
            }
        }
    }

    fn import_file(&mut self, path: PathBuf) {
        let final_path = if self.project_path.is_some() {
            self.assets
                .import_file(&path)
                .unwrap_or_else(|_| path.clone())
        } else {
            path.clone()
        };

        let mut cue = Cue::new_audio(final_path.clone());
        let dur = get_duration(&final_path);
        if dur > 0.0 {
            cue.duration_secs = dur;
            cue.loop_region.end_secs = dur;
        }
        cue.waveform = generate_waveform(&final_path, 512);

        let name = cue.name.clone();
        self.cue_list.add(cue);
        self.selected = self.cue_list.cues.last().map(|c| c.id);
        self.status_msg = format!("Added audio cue: {name}");
    }

    fn add_typed_cue(&mut self, cue_type: CueType) {
        let is_midi = matches!(&cue_type, CueType::Midi { .. });
        let cue = Cue::new_typed(cue_type);
        let label = cue.name.clone();
        self.cue_list.add(cue);
        self.selected = self.cue_list.cues.last().map(|c| c.id);
        if is_midi {
            self.active_panel = Panel::Midi;
        } else {
            self.active_panel = Panel::Inspector;
        }
        self.status_msg = format!("Added {label} event");
    }

    fn delete_cue(&mut self, id: Uuid) {
        self.audio.send(AudioCommand::Stop(id));
        if self.selected == Some(id) {
            self.selected = None;
        }
        // Убираем id из всех групп
        for c in &mut self.cue_list.cues {
            if let CueType::Group { children, .. } = &mut c.cue_type {
                children.retain(|x| *x != id);
            }
        }
        self.cue_list.remove(id);
        self.expanded_groups.remove(&id);
        self.active_playlists.retain(|p| p.group_id != id);
    }

    fn select_next(&mut self) {
        if self.cue_list.cues.is_empty() {
            return;
        }
        self.selected = Some(match self.selected {
            None => self.cue_list.cues[0].id,
            Some(cur) => {
                let pos = self
                    .cue_list
                    .cues
                    .iter()
                    .position(|c| c.id == cur)
                    .unwrap_or(0);
                self.cue_list.cues[(pos + 1).min(self.cue_list.cues.len() - 1)].id
            }
        });
    }

    fn select_prev(&mut self) {
        if self.cue_list.cues.is_empty() {
            return;
        }
        self.selected = Some(match self.selected {
            None => self.cue_list.cues[0].id,
            Some(cur) => {
                let pos = self
                    .cue_list
                    .cues
                    .iter()
                    .position(|c| c.id == cur)
                    .unwrap_or(0);
                self.cue_list.cues[pos.saturating_sub(1)].id
            }
        });
    }

    // ── Сохранение и загрузка проекта ────────────────────────────────────────

    fn save_project(&mut self) {
        if self.project_path.is_none() {
            let path = FileDialog::new()
                .add_filter("CueLabs Project", &["clabs"])
                .save_file();
            match path {
                Some(p) => {
                    let p = if p.extension().is_none() {
                        p.with_extension("clabs")
                    } else {
                        p
                    };
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
            Err(e) => {
                self.status_msg = format!("Save error: {e}");
                error!("{e}");
            }
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
                    let mut cl = proj.cue_list.clone();
                    for cue in &mut cl.cues {
                        if !cue.file_path.as_os_str().is_empty() {
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
                    }

                    self.cue_list = cl;
                    self.assets.set_project_path(path.clone());
                    self.project_path = Some(path.clone());
                    self.report = SessionReport::new(&proj.settings.name);
                    self.project = proj;
                    self.selected = None;
                    self.status_msg = format!("📂 Opened: {}", path.display());
                }
                Err(e) => {
                    self.status_msg = format!("Open error: {e}");
                    error!("{e}");
                }
            }
        }
    }

    fn new_project(&mut self) {
        self.stop_all();
        self.cue_list = CueList::new("Main");
        self.project = Project::default();
        self.project_path = None;
        self.report = SessionReport::new("Untitled Project");
        self.selected = None;
        self.status_msg = "New project".to_string();
    }

    // ── Опрос аудио, MIDI и OSC ──────────────────────────────────────────────

    fn poll_audio(&mut self) {
        let mut finished_auto_follow = Vec::new();
        // (group_id, child_id) — какие дети завершились, чтобы продвинуть плейлист
        let mut finished_playlist_child: Vec<(Uuid, Uuid)> = Vec::new();

        for upd in self.audio.drain_position_updates() {
            if let Some(cue) = self.cue_list.find_mut(upd.id) {
                let was_active = cue.is_active();
                cue.state = upd.state.clone();
                cue.position_secs = upd.position_secs;
                if upd.state == CueState::Stopped {
                    cue.position_secs = 0.0;
                    if was_active {
                        if cue.continue_mode == ContinueMode::AutoFollow {
                            finished_auto_follow.push(upd.id);
                        }
                        // Проверяем, является ли этот ребёнок текущим в каком-то плейлисте
                        for pl in &self.active_playlists {
                            if pl.children.get(pl.current_idx) == Some(&upd.id) {
                                finished_playlist_child.push((pl.group_id, upd.id));
                            }
                        }
                    }
                }
            }
        }

        for fin_id in finished_auto_follow {
            if let Some(next_id) = self.next_cue_id(fin_id) {
                self.trigger_cue(next_id);
            }
        }

        for (group_id, _child) in finished_playlist_child {
            if let Some(pos) = self
                .active_playlists
                .iter()
                .position(|p| p.group_id == group_id)
            {
                let mut pl = self.active_playlists.remove(pos);
                pl.current_idx += 1;
                if pl.current_idx < pl.children.len() {
                    let next_idx = pl.current_idx;
                    let fi = pl.fade_in_secs;
                    let fo = pl.fade_out_secs;
                    let kids = pl.children.clone();
                    self.start_playlist_child(group_id, kids, next_idx, fi, fo);
                } else {
                    self.status_msg = "📁 Playlist finished".to_string();
                }
            }
        }
    }

    fn poll_midi(&mut self) {
        for msg in self.midi.drain() {
            use crate::midi::input::MidiMessage;
            match msg {
                MidiMessage::NoteOn {
                    channel: _,
                    note,
                    velocity: _,
                } => {
                    if let Some(cue_id) = self
                        .cue_list
                        .cues
                        .iter()
                        .find(|c| {
                            matches!(
                                &c.midi_trigger,
                                Some(crate::cue::MidiTrigger::NoteOn { note: n, .. }) if *n == note
                            )
                        })
                        .map(|c| c.id)
                    {
                        self.trigger_cue(cue_id);
                    }
                }
                MidiMessage::CC {
                    channel: _,
                    cc,
                    value,
                } => {
                    if cc == 7 {
                        let vol = value as f32 / 127.0;
                        self.master_vol = vol;
                        for cue in self.cue_list.cues.iter().filter(|c| c.is_active()) {
                            self.audio
                                .send(AudioCommand::SetVolume(cue.id, cue.volume * vol));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn poll_osc(&mut self) {
        let events = match &self.osc {
            Some(osc) => osc.drain(),
            None => return,
        };
        for ev in events {
            match ev.addr.as_str() {
                "/go" | "/cue/go" => self.go(),
                "/stop" | "/cue/stop" => self.stop_all(),
                "/pause" | "/cue/pause" => self.pause_all(),
                "/resume" | "/cue/resume" => self.resume_all(),
                _ => {}
            }
        }
    }

    // ── Горячие клавиши и Drag-and-drop ──────────────────────────────────────

    fn handle_keyboard(&mut self, ctx: &Context) {
        if ctx.memory(|m| m.focused().is_some()) {
            return;
        }

        let keys: Vec<(Key, bool, bool, bool)> = ctx.input(|inp| {
            let mut v = Vec::new();
            for &k in &[
                Key::Space,
                Key::S,
                Key::Escape,
                Key::P,
                Key::R,
                Key::ArrowDown,
                Key::ArrowUp,
                Key::Delete,
                Key::N,
                Key::O,
                Key::I,
            ] {
                if inp.key_pressed(k) {
                    v.push((
                        k,
                        inp.modifiers.ctrl,
                        inp.modifiers.shift,
                        inp.modifiers.alt,
                    ));
                }
            }
            v
        });

        for (key, ctrl, shift, alt) in keys {
            match (key, ctrl, shift, alt) {
                (Key::Space, false, false, false) => self.go(),
                (Key::S, false, false, false) => self.stop_selected(),
                (Key::Escape, false, false, false) => self.stop_all(),
                (Key::P, false, false, false) => self.pause_selected(),
                (Key::P, false, true, false) => self.pause_all(),
                (Key::R, false, false, false) => self.resume_selected(),
                (Key::R, false, true, false) => self.resume_all(),
                (Key::ArrowDown, false, false, false) => self.select_next(),
                (Key::ArrowUp, false, false, false) => self.select_prev(),
                (Key::Delete, false, false, false) => {
                    if let Some(id) = self.selected {
                        self.delete_cue(id);
                    }
                }
                (Key::S, true, false, false) => self.save_project(),
                (Key::O, true, false, false) => self.open_project(),
                (Key::N, true, false, false) => self.new_project(),
                (Key::I, true, false, false) => self.add_files(),
                _ => {}
            }
        }
    }

    fn handle_drop(&mut self, ctx: &Context) {
        // dropped_files живёт ровно один кадр — клонируем СРАЗУ, до любых действий
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if dropped.is_empty() {
            return;
        }

        let audio_exts = ["wav", "mp3", "flac", "ogg", "aiff", "aif", "m4a"];
        let mut added = 0;
        for path in dropped {
            let is_audio = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| audio_exts.contains(&e.to_lowercase().as_str()))
                .unwrap_or(false);
            if !is_audio {
                tracing::warn!("Dropped non-audio file ignored: {:?}", path);
                continue;
            }
            tracing::info!("Dropped audio file: {:?}", path);
            self.import_file(path);
            added += 1;
        }

        if added > 0 {
            ctx.request_repaint();
            self.status_msg = format!("Added {} file(s) via drag-and-drop", added);
        }
    }

    // ── Верхнее меню ─────────────────────────────────────────────────────────

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::menu::bar(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("New project    Ctrl+N").clicked() {
                    self.new_project();
                    ui.close_menu();
                }
                if ui.button("Open…          Ctrl+O").clicked() {
                    self.open_project();
                    ui.close_menu();
                }
                if ui.button("Save           Ctrl+S").clicked() {
                    self.save_project();
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Add audio files… Ctrl+I").clicked() {
                    self.add_files();
                    ui.close_menu();
                }
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

            ui.menu_button("Cues", |ui| {
                if ui.button("🔊 Add Audio File…").clicked() {
                    self.add_files();
                    ui.close_menu();
                }
                if ui.button("▶ Add Play Cue").clicked() {
                    self.add_typed_cue(CueType::Play { target_id: None });
                    ui.close_menu();
                }
                if ui.button("⏸ Add Pause Cue").clicked() {
                    self.add_typed_cue(CueType::Pause { target_id: None });
                    ui.close_menu();
                }
                if ui.button("⏹ Add Stop Cue").clicked() {
                    self.add_typed_cue(CueType::Stop { target_id: None });
                    ui.close_menu();
                }
                if ui.button("↗ Add Fade In/Out Cue").clicked() {
                    self.add_typed_cue(CueType::Fade {
                        target_id: None,
                        fade_in: false,
                        duration_secs: 3.0,
                        stop_on_finish: true,
                    });
                    ui.close_menu();
                }
                if ui.button("🎹 Add MIDI Cue").clicked() {
                    self.add_typed_cue(CueType::Midi {
                        channel: 1,
                        message: MidiMessageKind::NoteOn {
                            note: 60,
                            velocity: 100,
                        },
                        raw_hex: "90 3C 64".to_string(),
                    });
                    ui.close_menu();
                }
                if ui.button("📁 Add Group Cue").clicked() {
                    self.add_typed_cue(CueType::Group {
                        mode: GroupMode::Normal,
                        children: Vec::new(),
                        fade_in_secs: 0.0,
                        fade_out_secs: 0.0,
                    });
                    ui.close_menu();
                }
            });

            ui.menu_button("View", |ui| {
                if ui.button("Inspector").clicked() {
                    self.active_panel = Panel::Inspector;
                    ui.close_menu();
                }
                if ui.button("Waveform & Loop").clicked() {
                    self.active_panel = Panel::Waveform;
                    ui.close_menu();
                }
                if ui.button("Playback").clicked() {
                    self.active_panel = Panel::Playback;
                    ui.close_menu();
                }
                if ui.button("I/O Routing").clicked() {
                    self.active_panel = Panel::IoRouting;
                    ui.close_menu();
                }
                if ui.button("Session Report").clicked() {
                    self.active_panel = Panel::Report;
                    ui.close_menu();
                }
                if ui.button("Settings").clicked() {
                    self.active_panel = Panel::Settings;
                    ui.close_menu();
                }
            });
        });
    }

    // ── Нижняя панель (Inspector / Waveform / Playback / I/O Routing / MIDI) ──

    fn bottom_panel(&mut self, ui: &mut egui::Ui) {
        let selected_is_midi = self
            .selected
            .and_then(|id| self.cue_list.find(id))
            .map(|c| matches!(&c.cue_type, CueType::Midi { .. }))
            .unwrap_or(false);

        if !selected_is_midi && self.active_panel == Panel::Midi {
            self.active_panel = Panel::Inspector;
        }

        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.active_panel, Panel::Inspector, "⚙ Inspector");
            ui.selectable_value(
                &mut self.active_panel,
                Panel::Waveform,
                "〰 Waveform & Region/Loop",
            );
            ui.selectable_value(&mut self.active_panel, Panel::Playback, "🎚 Playback");
            ui.selectable_value(&mut self.active_panel, Panel::IoRouting, "🔌 I/O Routing");
            if selected_is_midi {
                ui.selectable_value(&mut self.active_panel, Panel::Midi, "🎹 MIDI Settings");
            }
            ui.selectable_value(&mut self.active_panel, Panel::Report, "📋 Report");
            ui.selectable_value(&mut self.active_panel, Panel::Settings, "🔧 Settings");
        });
        ui.separator();

        match self.active_panel {
            Panel::Inspector => {
                let audio_targets = self.cue_list.audio_targets();
                let all_cues: Vec<(Uuid, String)> = self
                    .cue_list
                    .cues
                    .iter()
                    .map(|c| {
                        (
                            c.id,
                            format!("#{} — {} ({})", c.number, c.name, c.cue_type.label()),
                        )
                    })
                    .collect();

                if let Some(id) = self.selected {
                    if let Some(cue) = self.cue_list.find_mut(id) {
                        Inspector::show_inspector(ui, cue, &audio_targets, &all_cues);
                    }
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new("Select a cue to inspect its properties")
                                .color(Theme::TEXT_DIM),
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
                            egui::RichText::new("Select an audio cue to edit waveform & loop")
                                .color(Theme::TEXT_DIM),
                        );
                    });
                }
            }

            Panel::Playback => {
                if let Some(id) = self.selected {
                    let master = self.master_vol;
                    let mut new_vol = None;
                    if let Some(cue) = self.cue_list.find_mut(id) {
                        if Inspector::show_playback(ui, cue) {
                            new_vol = Some(cue.volume * master);
                        }
                    }
                    if let Some(v) = new_vol {
                        self.audio.send(AudioCommand::SetVolume(id, v));
                    }
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new("Select a cue to configure Playback & Fades")
                                .color(Theme::TEXT_DIM),
                        );
                    });
                }
            }

            Panel::IoRouting => {
                let cue_opt = self.selected.and_then(|id| self.cue_list.find_mut(id));
                Inspector::show_io_routing(ui, cue_opt, &mut self.audio);
            }

            Panel::Midi => {
                if let Some(id) = self.selected {
                    if let Some(cue) = self.cue_list.find_mut(id) {
                        Inspector::show_midi(ui, cue, &mut self.midi_out);
                    }
                }
            }

            Panel::Report => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let text = self.report.to_text_summary();
                    ui.label(
                        egui::RichText::new(text)
                            .monospace()
                            .size(11.0)
                            .color(Theme::TEXT_DIM),
                    );
                });
            }

            Panel::Settings => self.settings_panel(ui),
        }
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.columns(2, |cols| {
                cols[0].heading("Project");
                cols[0].horizontal(|ui| {
                    ui.label("Name:");
                    ui.text_edit_singleline(&mut self.project.settings.name);
                });
                cols[0].horizontal(|ui| {
                    ui.label("Author:");
                    ui.text_edit_singleline(&mut self.project.settings.author);
                });

                cols[0].add_space(6.0);
                cols[0].heading("OSC");
                cols[0].horizontal(|ui| {
                    ui.label("Listen port:");
                    ui.add(egui::DragValue::new(
                        &mut self.project.settings.osc_listen_port,
                    ));
                });
                cols[0].horizontal(|ui| {
                    ui.label("Send to:");
                    ui.text_edit_singleline(&mut self.project.settings.osc_send_addr);
                });

                cols[0].add_space(6.0);
                cols[0].heading("MIDI Input Ports");
                if self.midi.port_names.is_empty() {
                    cols[0].label(
                        egui::RichText::new("No MIDI input ports detected").color(Theme::TEXT_DIM),
                    );
                } else {
                    for p in &self.midi.port_names {
                        cols[0].label(format!("• {p}"));
                    }
                }

                cols[1].heading("Hotkeys");
                for (action, key) in &self.hotkeys.bindings {
                    cols[1].horizontal(|ui| {
                        ui.label(format!("{:<24}", action.label()));
                        ui.monospace(key);
                    });
                }
            });
        });
    }
}

impl eframe::App for CueLabsApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.poll_pending_triggers();
        self.poll_audio();
        self.poll_midi();
        self.poll_osc();
        self.handle_keyboard(ctx);
        self.handle_drop(ctx);

        if self.cue_list.active_count() > 0 || !self.pending_triggers.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
        }

        // ── Верхнее меню ──────────────────────────────────────────────────
        egui::TopBottomPanel::top("menu")
            .frame(
                egui::Frame::NONE
                    .fill(Theme::BG_PANEL)
                    .inner_margin(egui::Margin::symmetric(4_i8, 2_i8)),
            )
            .show(ctx, |ui| {
                self.menu_bar(ui);
            });

        // ── Статус-бар ────────────────────────────────────────────────────
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::NONE
                    .fill(Theme::BG_DARK)
                    .inner_margin(egui::Margin::symmetric(8_i8, 2_i8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&self.status_msg)
                            .color(Theme::TEXT_DIM)
                            .size(11.0),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "🔊 Out: {} | 🎙 In: {}",
                                self.audio.current_device, self.audio.current_input_device
                            ))
                            .color(Theme::TEXT_DIM)
                            .size(11.0),
                        );
                    });
                });
            });

        // ── Панель транспорта ─────────────────────────────────────────────
        egui::TopBottomPanel::bottom("transport")
            .frame(
                egui::Frame::NONE
                    .fill(Theme::BG_PANEL)
                    .inner_margin(egui::Margin::symmetric(8_i8, 6_i8)),
            )
            .show(ctx, |ui| {
                let active = self.cue_list.active_count();
                let prev_master = self.master_vol;
                let action = TransportBar::show(ui, active, &mut self.master_vol);
                if (self.master_vol - prev_master).abs() > f32::EPSILON {
                    for cue in self.cue_list.cues.iter().filter(|c| c.is_active()) {
                        self.audio.send(AudioCommand::SetVolume(
                            cue.id,
                            cue.volume * self.master_vol,
                        ));
                    }
                }
                match action {
                    Some(TransportAction::Go) => self.go(),
                    Some(TransportAction::StopSelected) => self.stop_selected(),
                    Some(TransportAction::StopAll) => self.stop_all(),
                    Some(TransportAction::PauseSelected) => self.pause_selected(),
                    Some(TransportAction::PauseAll) => self.pause_all(),
                    Some(TransportAction::ResumeSelected) => self.resume_selected(),
                    Some(TransportAction::ResumeAll) => self.resume_all(),
                    None => {}
                }
            });

        // ── Нижняя панель инспектора / вейвформы / роутинга / MIDI ────────
        egui::TopBottomPanel::bottom("bottom_inspector")
            .resizable(true)
            .min_height(160.0)
            .default_height(225.0)
            .frame(
                egui::Frame::NONE
                    .fill(Theme::BG_PANEL)
                    .inner_margin(egui::Margin::same(8_i8)),
            )
            .show(ctx, |ui| {
                self.bottom_panel(ui);
            });

        // ── Центральный список событий (Cue List) ─────────────────────────
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(Theme::BG_DARK)
                    .inner_margin(egui::Margin::same(0_i8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    let title = if self.project.settings.name.is_empty() {
                        "Untitled Project"
                    } else {
                        &self.project.settings.name
                    };

                    if self.renaming_project {
                        let resp = ui.text_edit_singleline(&mut self.project_name_buf);
                        resp.request_focus();
                        if resp.lost_focus() || ui.input(|i| i.key_pressed(Key::Enter)) {
                            self.project.settings.name = self.project_name_buf.clone();
                            self.renaming_project = false;
                        }
                    } else {
                        let title_resp =
                            ui.heading(egui::RichText::new(title).color(Theme::TEXT).size(14.0));
                        // Двойной клик по названию проекта меняет название
                        if title_resp.double_clicked() {
                            self.project_name_buf = self.project.settings.name.clone();
                            self.renaming_project = true;
                        }
                    }

                    if self.renaming_project {
                        if ui.input(|i| i.pointer.any_click()) {
                            if ctx.memory(|m| m.focused()).is_none() {
                                self.project.settings.name = self.project_name_buf.clone();
                                self.renaming_project = false;
                            }
                        }
                    }

                    ui.add_space(16.0);

                    if ui.button("🔊 Audio").clicked() {
                        self.add_files();
                    }
                    if ui.button("▶ Play").clicked() {
                        self.add_typed_cue(CueType::Play { target_id: None });
                    }
                    if ui.button("⏸ Pause").clicked() {
                        self.add_typed_cue(CueType::Pause { target_id: None });
                    }
                    if ui.button("⏹ Stop").clicked() {
                        self.add_typed_cue(CueType::Stop { target_id: None });
                    }
                    if ui.button("↗ Fade").clicked() {
                        self.add_typed_cue(CueType::Fade {
                            target_id: None,
                            fade_in: false,
                            duration_secs: 3.0,
                            stop_on_finish: true,
                        });
                    }
                    if ui.button("🎹 MIDI").clicked() {
                        self.add_typed_cue(CueType::Midi {
                            channel: 1,
                            message: MidiMessageKind::NoteOn {
                                note: 60,
                                velocity: 100,
                            },
                            raw_hex: "90 3C 64".to_string(),
                        });
                    }
                    if ui.button("📁 Group").clicked() {
                        // Создание группы из выбранного кью
                        let sel = self.selected;
                        self.add_typed_cue(CueType::Group {
                            mode: GroupMode::Normal,
                            children: sel.into_iter().collect(),
                            fade_in_secs: 0.0,
                            fade_out_secs: 0.0,
                        });
                    }
                });
                ui.add(egui::Separator::default().horizontal());

                let action = CueTable::show(
                    ui,
                    &mut self.cue_list.cues,
                    &mut self.selected,
                    &mut self.editing,
                    &mut self.expanded_groups,
                );

                if let Some(action) = action {
                    match action {
                        CueTableAction::Play(id) => {
                            self.selected = Some(id);
                            self.trigger_cue(id);
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
                        CueTableAction::MoveUp(id) => {
                            self.cue_list.move_up(id);
                        }
                        CueTableAction::MoveDown(id) => {
                            self.cue_list.move_down(id);
                        }
                        CueTableAction::StartEdit(id, field, buf) => {
                            self.editing = Some((id, field, buf));
                        }
                        CueTableAction::EditConfirm(id, field, new_val) => {
                            match field {
                                EditField::Name => {
                                    if let Some(cue) = self.cue_list.find_mut(id) {
                                        cue.name = new_val;
                                    }
                                }
                                EditField::PreWait => {
                                    if let Some(cue) = self.cue_list.find_mut(id) {
                                        if let Ok(v) = new_val.trim().parse::<f64>() {
                                            cue.pre_wait_secs = v.max(0.0);
                                        }
                                    }
                                }
                                EditField::PostWait => {
                                    if let Some(cue) = self.cue_list.find_mut(id) {
                                        if let Ok(v) = new_val.trim().parse::<f64>() {
                                            cue.post_wait_secs = v.max(0.0);
                                        }
                                    }
                                }
                            }
                            self.editing = None;
                        }
                        CueTableAction::EditCancel => {
                            self.editing = None;
                        }
                        CueTableAction::SetColor(id, color) => {
                            if let Some(cue) = self.cue_list.find_mut(id) {
                                cue.color = color;
                            }
                        }
                        CueTableAction::MoveToGroupOrIndex(dragged_id, target_idx) => {
                            self.handle_drag_drop(dragged_id, target_idx);
                        }
                        CueTableAction::AssignTarget(dragged_id, target_id) => {
                            // Сначала (ДО взятия mutable borrow) достаём имя перетаскиваемого cue
                            let dragged_name = self
                                .cue_list
                                .find(dragged_id)
                                .map(|c| c.name.clone())
                                .unwrap_or_default();

                            let mut status_msg: Option<String> = None;
                            if let Some(target_cue) = self.cue_list.find_mut(target_id) {
                                match &mut target_cue.cue_type {
                                    CueType::Play { target_id: tid }
                                    | CueType::Pause { target_id: tid }
                                    | CueType::Stop { target_id: tid }
                                    | CueType::Fade { target_id: tid, .. } => {
                                        *tid = Some(dragged_id);
                                        status_msg = Some(format!(
                                            "Target set: #{} → {}",
                                            target_cue.number, dragged_name
                                        ));
                                    }
                                    _ => {}
                                }
                            }
                            if let Some(msg) = status_msg {
                                self.status_msg = msg;
                            }
                        }
                    }
                }
            });
    }
}
