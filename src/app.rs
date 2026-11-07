// src/app.rs — главное состояние приложения CueLabs

use crate::audio::engine::{AudioCommand, AudioEngine};
use crate::audio::waveform::{generate_waveform, get_channel_count, get_duration};
use crate::config::{AppAction, AppSettings, AudioBackend, MidiControlBinding, MidiControlKind};
use crate::cue::{
    ContinueMode, Cue, CueList, CueState, CueType, FadeCurve, GroupMode, MidiMessageKind, OscCueArg,
};
use crate::export::session_report::SessionReport;
use crate::midi::input::{MidiInputManager, MidiOutputManager};
use crate::osc::server::OscServer;
use crate::project::{asset_manager::AssetManager, save_load::Project};
use crate::ui::{
    cue_table::{visible_cue_ids, CueTable, CueTableAction, EditField},
    inspector::{Inspector, InspectorAction},
    theme::Theme,
    transport::{TransportAction, TransportBar},
    waveform_editor::WaveformEditor,
};

use eframe::egui::{self, Context, Id, Key, RichText};
use rfd::FileDialog;
use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use uuid::Uuid;

fn ensure_project_extension(path: PathBuf) -> PathBuf {
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.eq_ignore_ascii_case("clabs"))
        .unwrap_or(false)
    {
        path
    } else {
        path.with_extension("clabs")
    }
}

fn project_name_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("Untitled Project")
        .to_string()
}

fn paths_refer_to_same_location(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn sanitize_project_name(name: &str) -> Option<String> {
    let sanitized: String = name
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    *character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
        })
        .collect();
    let sanitized = sanitized
        .trim()
        .trim_end_matches(|character: char| character == ' ' || character == '.')
        .trim()
        .to_string();

    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        None
    } else {
        Some(sanitized)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Panel {
    Inspector,
    Waveform,
    Playback,
    IoRouting,
    Midi,
    Report,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    Audio,
    Osc,
    Hotkeys,
    Midi,
    MidiControl,
    Playback,
    Backup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingAction {
    ExecuteCue,
    TriggerCue,
    CompleteCue,
}

#[derive(Debug, Clone)]
struct PendingTrigger {
    cue_id: Uuid,
    fire_at: Instant,
    action: PendingAction,
}

#[derive(Debug, Clone)]
struct ActivePlaylist {
    group_id: Uuid,
    children: Vec<Uuid>,
    current_idx: usize,
    fade_in_secs: f64,
    fade_out_secs: f64,
}

#[derive(Debug, Clone)]
struct ActiveAllCues {
    group_id: Uuid,
    remaining: HashSet<Uuid>,
}

/// Analyse a Normal group's automatic chain without changing runtime state.
/// The first child is started by the group. Each child whose Continue mode is
/// Auto-Continue / Auto-Follow automatically reaches the next child. A Nothing
/// before the last child is the manual boundary: open the group and select that
/// terminal automatically reached cue. A Nothing on the last child is simply
/// the end of the group, so keep it collapsed and let the caller select after it.
fn normal_group_manual_stop_in(
    cues: &[Cue],
    group_id: Uuid,
    visited: &mut HashSet<Uuid>,
) -> Option<(Vec<Uuid>, Uuid)> {
    if !visited.insert(group_id) {
        return None;
    }
    let group = cues.iter().find(|cue| cue.id == group_id)?;
    let CueType::Group {
        mode: GroupMode::Normal,
        children,
        ..
    } = &group.cue_type
    else {
        return None;
    };

    // Ignore stale child IDs when deciding whether Nothing is the final
    // reachable event; only actual cues contribute to the group's sequence.
    let valid_children: Vec<&Cue> = children
        .iter()
        .filter_map(|child_id| cues.iter().find(|cue| cue.id == *child_id))
        .collect();

    for (index, child) in valid_children.iter().copied().enumerate() {
        if matches!(
            &child.cue_type,
            CueType::Group {
                mode: GroupMode::Normal,
                ..
            }
        ) {
            if let Some((mut path, terminal_id)) =
                normal_group_manual_stop_in(cues, child.id, visited)
            {
                path.insert(0, group_id);
                return Some((path, terminal_id));
            }
        }

        if child.continue_mode == ContinueMode::Nothing {
            if index + 1 < valid_children.len() {
                return Some((vec![group_id], child.id));
            }
            return None;
        }
    }
    None
}

pub struct CueLabsApp {
    audio: AudioEngine,
    midi: MidiInputManager,
    midi_out: MidiOutputManager,
    osc: Option<OscServer>,

    cue_list: CueList,
    project: Project,
    project_path: Option<PathBuf>,
    project_needs_save_as: bool,
    assets: AssetManager,
    report: SessionReport,
    settings: AppSettings,
    settings_open: bool,
    settings_tab: SettingsTab,
    hotkey_capture: Option<AppAction>,
    midi_learn_action: Option<AppAction>,
    midi_cc_pressed: HashSet<(u8, u8)>,

    selected: Option<Uuid>,
    selected_ids: HashSet<Uuid>,
    selection_anchor: Option<Uuid>,
    editing: Option<(Uuid, EditField, String)>,
    editing_focus_pending: bool,
    project_name_edit: Option<String>,
    project_name_edit_focus_pending: bool,
    expanded_groups: HashSet<Uuid>,
    active_panel: Panel,
    master_vol: f32,
    status_msg: String,
    cue_column_widths: [f32; 9],
    bottom_panel_height: f32,
    bottom_panel_dirty: bool,
    bottom_panel_visible: bool,
    notes_sidebar_open: bool,
    fade_started_at: HashMap<Uuid, Instant>,
    fade_managed_targets: HashSet<Uuid>,
    pending_triggers: Vec<PendingTrigger>,
    active_playlists: Vec<ActivePlaylist>,
    active_all_cues: Vec<ActiveAllCues>,
    startup_frame_logged: bool,
    last_periodic_backup_at: Instant,
    last_backup_fingerprint: Option<u64>,
    last_media_scan_at: Instant,
}

impl CueLabsApp {
    pub fn new(cc: &eframe::CreationContext) -> Self {
        Theme::apply(&cc.egui_ctx);

        let mut settings = AppSettings::load();
        let mut audio =
            AudioEngine::new_with_backend(settings.audio_backend).unwrap_or_else(|error| {
                warn!(
                    "Could not initialise {} backend: {}. Falling back to WASAPI/shared default.",
                    settings.audio_backend.label(),
                    error
                );
                settings.audio_backend = AudioBackend::Wasapi;
                AudioEngine::new().unwrap_or_else(|fallback_error| {
                    panic!("Cannot initialise audio engine: {fallback_error}");
                })
            });
        if let Some(device) = settings.audio_output_device.as_deref() {
            if audio.output_devices.iter().any(|name| name == device) {
                audio.set_output_device(device);
            }
        }
        if let Some(device) = settings.audio_input_device.as_deref() {
            if audio.input_devices.iter().any(|name| name == device) {
                audio.set_input_device(device);
            }
        }
        audio.set_output_channel_names(settings.output_channel_names.clone());
        audio.set_default_output_pairs(settings.default_output_pairs.clone());
        settings.output_channels.resize(
            audio.output_channels.max(1),
            crate::cue::OutputChannelSetting::default(),
        );
        settings
            .output_channels
            .truncate(audio.output_channels.max(1));
        audio.set_output_channel_settings(settings.output_channels.clone());

        let mut midi = MidiInputManager::new();
        midi.set_repaint_context(cc.egui_ctx.clone());
        if let Some(port) = settings.midi_input_port.as_deref() {
            midi.refresh_and_connect(Some(port));
        }
        let mut midi_out = MidiOutputManager::new();
        if let Some(port) = settings.midi_output_port.as_deref() {
            midi_out.refresh_and_connect(Some(port));
        }

        let osc = OscServer::new(settings.osc_listen_port, settings.osc_send_addr.clone())
            .map_err(|e| warn!("OSC unavailable: {e}"))
            .ok();
        let cue_column_widths = settings.cue_column_widths;
        let bottom_panel_height = settings.bottom_panel_height.clamp(80.0, 1200.0);
        info!("CueLabs initialization complete; creating the main window");

        Self {
            audio,
            midi,
            midi_out,
            osc,
            cue_list: CueList::new("Main"),
            project: Project::default(),
            project_path: None,
            project_needs_save_as: false,
            assets: AssetManager::new(),
            report: SessionReport::new("Untitled Project"),
            settings,
            settings_open: false,
            settings_tab: SettingsTab::Audio,
            hotkey_capture: None,
            midi_learn_action: None,
            midi_cc_pressed: HashSet::new(),
            selected: None,
            selected_ids: HashSet::new(),
            selection_anchor: None,
            editing: None,
            editing_focus_pending: false,
            project_name_edit: None,
            project_name_edit_focus_pending: false,
            expanded_groups: HashSet::new(),
            active_panel: Panel::Inspector,
            master_vol: 1.0,
            status_msg: "Ready — drag audio files here or add events from the toolbar".to_string(),
            pending_triggers: Vec::new(),
            active_playlists: Vec::new(),
            active_all_cues: Vec::new(),
            startup_frame_logged: false,
            last_periodic_backup_at: Instant::now(),
            last_backup_fingerprint: None,
            last_media_scan_at: Instant::now(),
            cue_column_widths,
            bottom_panel_height,
            bottom_panel_dirty: false,
            bottom_panel_visible: true,
            notes_sidebar_open: true,
            fade_started_at: HashMap::new(),
            fade_managed_targets: HashSet::new(),
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
        if self.selected.is_none() {
            self.select_only(id);
        }
        let next_manual = self.selection_after_auto_chain(id);
        self.trigger_cue(id);
        // A group may select its own manual boundary while executing. Preserve
        // that selection; otherwise advance past the entire automatic chain.
        if self.selected == Some(id) {
            if let Some(next_id) = next_manual {
                self.select_only(next_id);
            }
        }
    }

    fn selection_after_auto_chain(&self, start_id: Uuid) -> Option<Uuid> {
        let mut current_id = start_id;
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current_id) {
                return Some(current_id);
            }
            let Some(cue) = self.cue_list.find(current_id) else {
                return None;
            };
            if cue.continue_mode == ContinueMode::Nothing {
                return self.next_cue_id(current_id).or(Some(current_id));
            }
            let Some(next_id) = self.next_auto_cue_id(current_id) else {
                return Some(current_id);
            };
            current_id = next_id;
        }
    }

    fn trigger_cue(&mut self, id: Uuid) {
        let Some(cue) = self.cue_list.find(id) else {
            return;
        };

        let pre_wait = cue.pre_wait_secs.max(0.0);
        let post_wait = cue.post_wait_secs.max(0.0);
        let is_fade_action = matches!(&cue.cue_type, CueType::Fade { .. });
        let continue_mode = cue.continue_mode.clone();
        let is_group_run = matches!(
            &cue.cue_type,
            CueType::Group {
                mode: GroupMode::Playlist | GroupMode::AllCues,
                ..
            }
        );
        let is_managed_child = self.is_managed_group_child(id);
        let cue_name = cue.name.clone();

        if pre_wait > 0.0 {
            if let Some(cue) = self.cue_list.find_mut(id) {
                cue.state = CueState::Waiting;
            }
            self.pending_triggers.push(PendingTrigger {
                cue_id: id,
                fire_at: Instant::now() + std::time::Duration::from_secs_f64(pre_wait),
                action: PendingAction::ExecuteCue,
            });
            self.status_msg = format!("⏳ Pre-wait ({:.1}s): {}", pre_wait, cue_name);
        } else {
            self.execute_cue_now(id);
        }

        // Auto-Continue keeps its time-based behaviour in Normal mode. Playlist
        // and All Cues own their child order, so their children cannot schedule
        // a second, competing continuation chain.
        if continue_mode == ContinueMode::AutoContinue
            && !is_managed_child
            && !is_group_run
            && !is_fade_action
        {
            if let Some(next_id) = self.next_auto_cue_id(id) {
                let delay = pre_wait + post_wait;
                if delay > 0.0 {
                    self.pending_triggers.push(PendingTrigger {
                        cue_id: next_id,
                        fire_at: Instant::now() + std::time::Duration::from_secs_f64(delay),
                        action: PendingAction::TriggerCue,
                    });
                } else {
                    self.trigger_cue(next_id);
                }
            }
        }
    }

    fn is_managed_group_child(&self, cue_id: Uuid) -> bool {
        self.active_playlists
            .iter()
            .any(|run| run.children.contains(&cue_id))
            || self
                .active_all_cues
                .iter()
                .any(|run| run.remaining.contains(&cue_id))
    }

    /// Schedule the completion of a cue after its own post-wait. `extra_delay`
    /// is used by timed actions such as Fade, which must finish before post-wait.
    fn schedule_cue_completion(&mut self, cue_id: Uuid, extra_delay: f64) {
        let post_wait = self
            .cue_list
            .find(cue_id)
            .map(|cue| cue.post_wait_secs.max(0.0))
            .unwrap_or(0.0);
        let delay = extra_delay.max(0.0) + post_wait;
        if delay > 0.0 {
            self.pending_triggers.push(PendingTrigger {
                cue_id,
                fire_at: Instant::now() + std::time::Duration::from_secs_f64(delay),
                action: PendingAction::CompleteCue,
            });
        } else {
            self.complete_cue(cue_id);
        }
    }

    /// Called after an event has finished and its post-wait has elapsed.
    /// Playlist advances one child; All Cues waits for every child; Normal
    /// follows the cue's Auto-Follow setting.
    fn complete_cue(&mut self, cue_id: Uuid) {
        let suppress_target_continue = self.fade_managed_targets.remove(&cue_id);
        let fade_target_id = self.cue_list.find(cue_id).and_then(|cue| {
            if let CueType::Fade { target_id, .. } = &cue.cue_type {
                *target_id
            } else {
                None
            }
        });
        self.fade_started_at.remove(&cue_id);
        if let Some(cue) = self.cue_list.find_mut(cue_id) {
            if matches!(&cue.cue_type, CueType::Fade { .. }) {
                cue.state = CueState::Stopped;
                cue.position_secs = 0.0;
            }
        }
        if let Some(run_pos) = self
            .active_playlists
            .iter()
            .position(|run| run.children.get(run.current_idx) == Some(&cue_id))
        {
            let (group_id, next_idx, children, fade_in, fade_out, finished) = {
                let run = &mut self.active_playlists[run_pos];
                run.current_idx += 1;
                (
                    run.group_id,
                    run.current_idx,
                    run.children.clone(),
                    run.fade_in_secs,
                    run.fade_out_secs,
                    run.current_idx >= run.children.len(),
                )
            };
            if finished {
                self.active_playlists.remove(run_pos);
                self.status_msg = "📁 Playlist finished".to_string();
                self.schedule_cue_completion(group_id, 0.0);
            } else if let Some(next_child_id) = children.get(next_idx).copied() {
                // Queue the next child for the next UI/audio tick instead of
                // recursively triggering instant MIDI/command cues on the stack.
                self.apply_group_fades(next_child_id, fade_in, fade_out);
                self.select_only(next_child_id);
                self.status_msg = format!("📁 Playlist: event {}/{}", next_idx + 1, children.len());
                let already_started_as_fade_target = fade_target_id == Some(next_child_id)
                    && self
                        .cue_list
                        .find(next_child_id)
                        .map(|cue| cue.is_active())
                        .unwrap_or(false);
                if !already_started_as_fade_target {
                    self.pending_triggers.push(PendingTrigger {
                        cue_id: next_child_id,
                        fire_at: Instant::now(),
                        action: PendingAction::TriggerCue,
                    });
                }
            }
            return;
        }

        if let Some(run_pos) = self
            .active_all_cues
            .iter()
            .position(|run| run.remaining.contains(&cue_id))
        {
            let (group_id, finished) = {
                let run = &mut self.active_all_cues[run_pos];
                run.remaining.remove(&cue_id);
                (run.group_id, run.remaining.is_empty())
            };
            if finished {
                self.active_all_cues.remove(run_pos);
                self.status_msg = "📁 All Cues finished".to_string();
                self.schedule_cue_completion(group_id, 0.0);
            }
            return;
        }

        // The Fade cue owns continuation when it directly started or stopped
        // this target. Do not let the target fire a second automatic chain.
        if suppress_target_continue {
            return;
        }

        let Some(cue) = self.cue_list.find(cue_id) else {
            return;
        };
        let continue_mode = cue.continue_mode.clone();
        let is_managed_group_mode = matches!(
            &cue.cue_type,
            CueType::Group {
                mode: GroupMode::Playlist | GroupMode::AllCues,
                ..
            }
        );
        let should_follow = continue_mode == ContinueMode::AutoFollow
            || (is_managed_group_mode && continue_mode == ContinueMode::AutoContinue);

        if should_follow {
            if let Some(next_id) = self.next_auto_cue_id(cue_id) {
                self.trigger_cue(next_id);
            }
        }
    }

    /// Start an Audio cue. An optional override is used by a Fade In cue when
    /// its target is currently stopped, so the track starts from silence and
    /// reaches the requested level using the selected fade curve.
    fn start_audio_cue(
        &mut self,
        id: Uuid,
        fade_override: Option<(f64, FadeCurve, Vec<[f32; 2]>, f32)>,
    ) {
        let Some(snapshot) = self.cue_list.find(id).cloned() else {
            return;
        };
        if !matches!(&snapshot.cue_type, CueType::Audio) {
            return;
        }

        let source_channels = snapshot.audio_channels.max(1);
        let output_channels = self
            .audio
            .output_channels_for_device(snapshot.io.output_device.as_deref());
        let mut routing = snapshot.io.clone();
        let default_route_channels = self
            .audio
            .default_route_output_channels(snapshot.io.output_device.as_deref());
        routing.ensure_channel_routes_with_defaults(
            source_channels,
            output_channels,
            &default_route_channels,
        );
        if let Some(cue) = self.cue_list.find_mut(id) {
            cue.state = CueState::Playing;
            cue.audio_channels = source_channels;
            cue.io.channel_routes = routing.channel_routes.clone();
        }

        info!(
            "Starting audio cue {:?}: {} source channels, {} output channels",
            snapshot.name, source_channels, output_channels
        );
        let (volume, fade_in_secs, fade_curve, custom_fade_points) = match fade_override {
            Some((duration, curve, points, target_volume)) => {
                (target_volume, duration, curve, points)
            }
            None => (
                snapshot.volume * self.master_vol,
                snapshot.fade_in_secs,
                snapshot.fade_curve,
                snapshot.custom_fade_points.clone(),
            ),
        };

        self.audio.send(AudioCommand::Play {
            id: snapshot.id,
            file: snapshot.file_path.clone(),
            output_device: snapshot.io.output_device.clone(),
            start_secs: if snapshot.position_secs > snapshot.start_secs {
                snapshot
                    .position_secs
                    .clamp(snapshot.start_secs, snapshot.effective_end_secs())
            } else {
                snapshot.start_secs
            },
            end_secs: snapshot.end_secs,
            duration_secs: snapshot.duration_secs,
            volume,
            fade_in_secs,
            fade_out_secs: snapshot.fade_out_secs,
            fade_curve,
            custom_fade_points,
            channel_routes: routing.channel_routes,
            loop_enabled: snapshot.loop_region.enabled,
            loop_start: snapshot.loop_region.start_secs,
            loop_end: snapshot.loop_region.end_secs,
            loop_count: snapshot.loop_region.count,
        });
    }

    fn execute_cue_now(&mut self, id: Uuid) {
        let Some(cue_snapshot) = self.cue_list.find(id).cloned() else {
            return;
        };

        match cue_snapshot.cue_type {
            CueType::Audio => {
                self.start_audio_cue(id, None);
                self.report
                    .record(&cue_snapshot.number, &cue_snapshot.name, 0.0, "Play");
                self.status_msg = format!("▶ Playing: {}", cue_snapshot.name);
            }

            CueType::Play { target_id } => {
                if let Some(tid) = target_id {
                    self.execute_cue_now(tid);
                } else if !cue_snapshot.file_path.as_os_str().is_empty() {
                    let source_channels = cue_snapshot.audio_channels.max(1);
                    let track_output_channels = self
                        .audio
                        .output_channels_for_device(cue_snapshot.io.output_device.as_deref());
                    let mut io_routing = cue_snapshot.io.clone();
                    let default_route_channels = self
                        .audio
                        .default_route_output_channels(cue_snapshot.io.output_device.as_deref());
                    io_routing.ensure_channel_routes_with_defaults(
                        source_channels,
                        track_output_channels,
                        &default_route_channels,
                    );
                    if let Some(cue) = self.cue_list.find_mut(id) {
                        cue.state = CueState::Playing;
                        cue.audio_channels = source_channels;
                        cue.io.channel_routes = io_routing.channel_routes.clone();
                    }
                    self.audio.send(AudioCommand::Play {
                        id: cue_snapshot.id,
                        file: cue_snapshot.file_path.clone(),
                        output_device: cue_snapshot.io.output_device.clone(),
                        start_secs: if cue_snapshot.position_secs > cue_snapshot.start_secs {
                            cue_snapshot
                                .position_secs
                                .clamp(cue_snapshot.start_secs, cue_snapshot.effective_end_secs())
                        } else {
                            cue_snapshot.start_secs
                        },
                        end_secs: cue_snapshot.end_secs,
                        duration_secs: cue_snapshot.duration_secs,
                        volume: cue_snapshot.volume * self.master_vol,
                        fade_in_secs: cue_snapshot.fade_in_secs,
                        fade_out_secs: cue_snapshot.fade_out_secs,
                        fade_curve: cue_snapshot.fade_curve,
                        custom_fade_points: cue_snapshot.custom_fade_points.clone(),
                        channel_routes: io_routing.channel_routes,
                        loop_enabled: cue_snapshot.loop_region.enabled,
                        loop_start: cue_snapshot.loop_region.start_secs,
                        loop_end: cue_snapshot.loop_region.end_secs,
                        loop_count: cue_snapshot.loop_region.count,
                    });
                }
                self.schedule_cue_completion(id, 0.0);
            }

            CueType::Pause { target_id } => {
                if let Some(tid) = target_id {
                    let targets = self.audio_targets_in(tid);
                    for audio_id in targets {
                        if let Some(target) = self.cue_list.find_mut(audio_id) {
                            target.state = CueState::Paused;
                        }
                        self.audio.send(AudioCommand::Pause(audio_id));
                    }
                    if let Some(target) = self.cue_list.find_mut(tid) {
                        target.state = CueState::Paused;
                    }
                } else {
                    self.pause_all();
                }
                self.schedule_cue_completion(id, 0.0);
            }

            CueType::Stop { target_id } => {
                if let Some(tid) = target_id {
                    let was_managed_child = self.is_managed_group_child(tid);
                    for audio_id in self.audio_targets_in(tid) {
                        self.fade_managed_targets.remove(&audio_id);
                    }
                    self.stop_target_and_descendants(tid);
                    if was_managed_child {
                        self.schedule_cue_completion(tid, 0.0);
                    }
                } else {
                    self.stop_all();
                }
                self.schedule_cue_completion(id, 0.0);
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
                self.fade_started_at.remove(&id);
                if let Some(tid) = target_id {
                    let targets = self.audio_targets_in(tid);
                    let mut fade_started = false;
                    let mut fade_blocked_by_continue = false;
                    for audio_id in targets {
                        let Some(target) = self.cue_list.find(audio_id).cloned() else {
                            continue;
                        };
                        let target_vol = target_volume.unwrap_or_else(|| {
                            if fade_in {
                                // A cue muted at -60 dB has a linear volume of
                                // zero. In the absence of an explicit Fade target,
                                // use unity so Fade In can actually raise it.
                                if target.volume <= 0.000_001 {
                                    1.0
                                } else {
                                    target.volume
                                }
                            } else {
                                0.0
                            }
                        }) * self.master_vol;
                        let stopped =
                            matches!(&target.state, CueState::Stopped | CueState::Error(_));
                        let paused = target.state == CueState::Paused;
                        let playing = matches!(
                            &target.state,
                            CueState::Playing | CueState::FadingIn | CueState::FadingOut
                        );

                        if fade_in
                            && (stopped || paused)
                            && cue_snapshot.continue_mode == ContinueMode::Nothing
                        {
                            self.status_msg = format!(
                                "Fade In not started: '{}' is stopped/paused and Continue is Nothing",
                                target.name
                            );
                            fade_blocked_by_continue = true;
                            continue;
                        }

                        if fade_in && stopped {
                            if cue_snapshot.continue_mode != ContinueMode::Nothing {
                                self.fade_managed_targets.insert(audio_id);
                            }
                            self.start_audio_cue(
                                audio_id,
                                Some((
                                    duration_secs,
                                    fade_curve,
                                    custom_fade_points.clone(),
                                    target_vol,
                                )),
                            );
                            if let Some(target) = self.cue_list.find_mut(audio_id) {
                                target.state = CueState::FadingIn;
                            }
                            fade_started = true;
                        } else if playing || (fade_in && paused) {
                            if (!fade_in && stop_on_finish) || (fade_in && paused) {
                                self.fade_managed_targets.insert(audio_id);
                            }
                            if fade_in && paused {
                                self.audio.send(AudioCommand::Resume(audio_id));
                            }
                            self.audio.send(AudioCommand::Fade {
                                id: audio_id,
                                target_volume: target_vol,
                                duration_secs,
                                stop_on_finish: !fade_in && stop_on_finish,
                                curve: fade_curve,
                                custom_fade_points: custom_fade_points.clone(),
                            });
                            if let Some(target) = self.cue_list.find_mut(audio_id) {
                                target.state = if fade_in {
                                    CueState::FadingIn
                                } else {
                                    CueState::FadingOut
                                };
                            }
                            fade_started = true;
                        }
                    }
                    if fade_started {
                        if let Some(fade_cue) = self.cue_list.find_mut(id) {
                            fade_cue.state = if fade_in {
                                CueState::FadingIn
                            } else {
                                CueState::FadingOut
                            };
                            // `duration_secs` and `position_secs` are runtime-only fields
                            // for the status cell; do not persist an audio End marker on a Fade cue.
                            fade_cue.duration_secs = duration_secs.max(0.0);
                            fade_cue.position_secs = 0.0;
                        }
                        self.fade_started_at.insert(id, Instant::now());
                        self.status_msg = format!(
                            "{}: {} · {:.1}s",
                            if fade_in {
                                "↗ Fade In"
                            } else {
                                "↘ Fade Out"
                            },
                            cue_snapshot.name,
                            duration_secs
                        );

                        // Fade is a timed cue. Auto-Continue starts the next
                        // independent cue only after the fade and post-wait;
                        // the target audio cue is never mistaken for that next cue.
                        let auto_continue_next = if cue_snapshot.continue_mode
                            == ContinueMode::AutoContinue
                            && !self.is_managed_group_child(id)
                        {
                            self.next_auto_cue_id(id)
                        } else {
                            None
                        };
                        self.schedule_cue_completion(id, duration_secs);
                        if let Some(next_id) = auto_continue_next {
                            let delay =
                                duration_secs.max(0.0) + cue_snapshot.post_wait_secs.max(0.0);
                            self.pending_triggers.push(PendingTrigger {
                                cue_id: next_id,
                                fire_at: Instant::now() + std::time::Duration::from_secs_f64(delay),
                                action: PendingAction::TriggerCue,
                            });
                        }
                    } else {
                        self.fade_started_at.remove(&id);
                        if !fade_blocked_by_continue {
                            self.status_msg = format!("{}: no active target", cue_snapshot.name);
                        }
                        if let Some(fade_cue) = self.cue_list.find_mut(id) {
                            fade_cue.state = CueState::Stopped;
                            fade_cue.position_secs = 0.0;
                        }
                    }
                } else {
                    self.status_msg = format!("{}: no target selected", cue_snapshot.name);
                    if let Some(fade_cue) = self.cue_list.find_mut(id) {
                        fade_cue.state = CueState::Stopped;
                        fade_cue.position_secs = 0.0;
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
                self.schedule_cue_completion(id, 0.0);
            }

            CueType::Osc { address, args } => {
                let osc_args = args
                    .into_iter()
                    .map(|arg| match arg {
                        OscCueArg::Int(value) => rosc::OscType::Int(value),
                        OscCueArg::Float(value) => rosc::OscType::Float(value),
                        OscCueArg::String(value) => rosc::OscType::String(value),
                        OscCueArg::Bool(value) => rosc::OscType::Bool(value),
                    })
                    .collect();
                if let Some(server) = &self.osc {
                    server.send(&address, osc_args);
                    self.status_msg = format!("📡 OSC sent: {address}");
                    self.report
                        .record(&cue_snapshot.number, &cue_snapshot.name, 0.0, "OSC");
                } else {
                    self.status_msg = "OSC is not available; check Settings → OSC".to_string();
                }
                self.schedule_cue_completion(id, 0.0);
            }

            CueType::Group {
                mode,
                children,
                fade_in_secs,
                fade_out_secs,
            } => {
                match mode {
                    GroupMode::Normal => {
                        if let Some(first_id) = children.first().copied() {
                            if let Some((groups_to_expand, terminal_id)) =
                                self.normal_group_manual_stop(id, &mut HashSet::new())
                            {
                                for group_id in groups_to_expand {
                                    self.expanded_groups.insert(group_id);
                                }
                                // Keep the selection on the final automatically reached cue
                                // when there are still manual cues after it inside the group.
                                self.select_only(terminal_id);
                                self.status_msg = "📁 Group: automatic chain stops here; remaining cues need manual GO".to_string();
                            } else {
                                // The automatic chain reaches the end of this group. Do not
                                // auto-expand it; move the operator cursor to the next root/parent item.
                                self.expanded_groups.remove(&id);
                                if let Some(next_id) = self.next_after_container(id) {
                                    self.select_only(next_id);
                                } else {
                                    self.select_only(id);
                                }
                                self.status_msg =
                                    "📁 Group (Normal): automatic chain reaches group end"
                                        .to_string();
                            }
                            self.trigger_cue(first_id);
                        } else {
                            self.status_msg = "📁 Group is empty".to_string();
                        }
                        self.schedule_cue_completion(id, 0.0);
                    }
                    GroupMode::Playlist => {
                        if children.is_empty() {
                            self.status_msg = "📁 Playlist is empty".to_string();
                            self.schedule_cue_completion(id, 0.0);
                        } else {
                            self.active_playlists.retain(|run| run.group_id != id);
                            self.active_playlists.push(ActivePlaylist {
                                group_id: id,
                                children: children.clone(),
                                current_idx: 0,
                                fade_in_secs,
                                fade_out_secs,
                            });
                            self.start_playlist_child(id, children, 0, fade_in_secs, fade_out_secs);
                        }
                    }
                    GroupMode::AllCues => {
                        let valid_children: Vec<Uuid> = children
                            .into_iter()
                            .filter(|child_id| self.cue_list.find(*child_id).is_some())
                            .collect();
                        if valid_children.is_empty() {
                            self.status_msg = "📁 All Cues group is empty".to_string();
                            self.schedule_cue_completion(id, 0.0);
                        } else {
                            self.active_all_cues.retain(|run| run.group_id != id);
                            self.active_all_cues.push(ActiveAllCues {
                                group_id: id,
                                remaining: valid_children.iter().copied().collect(),
                            });
                            self.status_msg =
                                format!("📁 All Cues: starting {} events", valid_children.len());
                            for child_id in valid_children {
                                self.apply_group_fades(child_id, fade_in_secs, fade_out_secs);
                                let already_running = self
                                    .cue_list
                                    .find(child_id)
                                    .map(|cue| cue.is_active())
                                    .unwrap_or(false);
                                if !already_running {
                                    self.trigger_cue(child_id);
                                }
                            }
                        }
                    }
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

        if let Some(run) = self
            .active_playlists
            .iter_mut()
            .find(|run| run.group_id == group_id)
        {
            run.current_idx = idx;
        } else {
            self.active_playlists.push(ActivePlaylist {
                group_id,
                children: children.clone(),
                current_idx: idx,
                fade_in_secs: fade_in,
                fade_out_secs: fade_out,
            });
        }

        self.apply_group_fades(child_id, fade_in, fade_out);
        self.select_only(child_id);
        self.trigger_cue(child_id);
        self.status_msg = format!("📁 Playlist: event {}/{}", idx + 1, children.len());
    }

    fn apply_group_fades(&mut self, child_id: Uuid, fade_in: f64, fade_out: f64) {
        if let Some(child) = self.cue_list.find_mut(child_id) {
            if fade_in > 0.0 {
                child.fade_in_secs = fade_in;
            }
            if fade_out > 0.0 {
                child.fade_out_secs = fade_out;
            }
        }
    }

    /// Returns the expanded group path and the final automatically reached cue
    /// when a Normal group's chain stops before its last child. A Nothing on the
    /// final child means the group is fully consumed and must remain collapsed.
    fn normal_group_manual_stop(
        &self,
        group_id: Uuid,
        visited: &mut HashSet<Uuid>,
    ) -> Option<(Vec<Uuid>, Uuid)> {
        normal_group_manual_stop_in(&self.cue_list.cues, group_id, visited)
    }

    fn parent_group_of(&self, cue_id: Uuid) -> Option<Uuid> {
        self.cue_list.cues.iter().find_map(|cue| {
            if let CueType::Group { children, .. } = &cue.cue_type {
                children.contains(&cue_id).then_some(cue.id)
            } else {
                None
            }
        })
    }

    fn group_descendant_ids(&self, root_id: Uuid) -> Vec<Uuid> {
        let mut result = Vec::new();
        let mut stack = vec![root_id];
        let mut visited = HashSet::new();
        while let Some(id) = stack.pop() {
            if !visited.insert(id) {
                continue;
            }
            result.push(id);
            if let Some(cue) = self.cue_list.find(id) {
                if let CueType::Group { children, .. } = &cue.cue_type {
                    stack.extend(children.iter().copied());
                }
            }
        }
        result
    }

    fn audio_targets_in(&self, target_id: Uuid) -> Vec<Uuid> {
        self.group_descendant_ids(target_id)
            .into_iter()
            .filter(|id| {
                self.cue_list
                    .find(*id)
                    .map(|cue| matches!(&cue.cue_type, CueType::Audio))
                    .unwrap_or(false)
            })
            .collect()
    }

    fn stop_target_and_descendants(&mut self, target_id: Uuid) {
        let affected = self.group_descendant_ids(target_id);
        self.pending_triggers
            .retain(|trigger| !affected.contains(&trigger.cue_id));
        self.active_playlists
            .retain(|run| !affected.contains(&run.group_id));
        self.active_all_cues
            .retain(|run| !affected.contains(&run.group_id));
        let audio_ids = self.audio_targets_in(target_id);
        for id in affected {
            if let Some(cue) = self.cue_list.find_mut(id) {
                cue.state = CueState::Stopped;
                cue.position_secs = 0.0;
            }
        }
        for id in audio_ids {
            self.audio.send(AudioCommand::Stop(id));
        }
    }

    fn group_contains(&self, root_group: Uuid, target_id: Uuid) -> bool {
        let mut stack = vec![root_group];
        let mut visited = HashSet::new();
        while let Some(group_id) = stack.pop() {
            if !visited.insert(group_id) {
                continue;
            }
            if group_id == target_id {
                return true;
            }
            if let Some(cue) = self.cue_list.find(group_id) {
                if let CueType::Group { children, .. } = &cue.cue_type {
                    stack.extend(children.iter().copied());
                }
            }
        }
        false
    }

    fn handle_drag_drop_position(
        &mut self,
        dragged_ids: Vec<Uuid>,
        target_parent: Option<Uuid>,
        sibling_index: usize,
    ) {
        let mut ordered_ids = Vec::new();
        for dragged_id in dragged_ids {
            if self.cue_list.find(dragged_id).is_some() && !ordered_ids.contains(&dragged_id) {
                ordered_ids.push(dragged_id);
            }
        }
        if ordered_ids.is_empty() {
            return;
        }

        // If a group and one of its descendants are selected, move only the group.
        let selected_group_ids: Vec<Uuid> = ordered_ids
            .iter()
            .copied()
            .filter(|id| {
                self.cue_list
                    .find(*id)
                    .map(|cue| matches!(&cue.cue_type, CueType::Group { .. }))
                    .unwrap_or(false)
            })
            .collect();
        ordered_ids.retain(|id| {
            !selected_group_ids
                .iter()
                .any(|group_id| group_id != id && self.group_contains(*group_id, *id))
        });
        if ordered_ids.is_empty() {
            return;
        }

        if let Some(parent_group_id) = target_parent {
            if ordered_ids.contains(&parent_group_id)
                || ordered_ids.iter().any(|dragged_id| {
                    self.cue_list
                        .find(*dragged_id)
                        .map(|cue| matches!(&cue.cue_type, CueType::Group { .. }))
                        .unwrap_or(false)
                        && self.group_contains(*dragged_id, parent_group_id)
                })
            {
                self.status_msg =
                    "Cannot move a group into itself or one of its descendants".to_string();
                return;
            }

            let Some(parent_snapshot) = self.cue_list.find(parent_group_id).cloned() else {
                return;
            };
            let CueType::Group {
                children: original_children,
                ..
            } = parent_snapshot.cue_type
            else {
                return;
            };
            let removed_before = original_children
                .iter()
                .take(sibling_index.min(original_children.len()))
                .filter(|id| ordered_ids.contains(*id))
                .count();
            let insert_at = sibling_index.saturating_sub(removed_before);

            for cue in &mut self.cue_list.cues {
                if let CueType::Group { children, .. } = &mut cue.cue_type {
                    children.retain(|child_id| !ordered_ids.contains(child_id));
                }
            }
            if let Some(parent) = self.cue_list.find_mut(parent_group_id) {
                if let CueType::Group { children, .. } = &mut parent.cue_type {
                    for (offset, dragged_id) in ordered_ids.iter().copied().enumerate() {
                        children.insert((insert_at + offset).min(children.len()), dragged_id);
                    }
                }
            }
            self.expanded_groups.insert(parent_group_id);
            self.status_msg = format!("Moved {} event(s) into group", ordered_ids.len());
            return;
        }

        // Root-level ordering is derived from cues that are not children of any
        // group. Convert the requested sibling index to the backing vector index.
        let child_ids: HashSet<Uuid> = self
            .cue_list
            .cues
            .iter()
            .filter_map(|cue| {
                if let CueType::Group { children, .. } = &cue.cue_type {
                    Some(children.iter().copied())
                } else {
                    None
                }
            })
            .flatten()
            .collect();
        let root_ids: Vec<Uuid> = self
            .cue_list
            .cues
            .iter()
            .filter(|cue| !child_ids.contains(&cue.id))
            .map(|cue| cue.id)
            .collect();
        let target_root_id = root_ids
            .iter()
            .skip(sibling_index.min(root_ids.len()))
            .find(|id| !ordered_ids.contains(*id))
            .copied();
        let target_backing_index = target_root_id
            .and_then(|target_id| {
                self.cue_list
                    .cues
                    .iter()
                    .position(|cue| cue.id == target_id)
            })
            .unwrap_or(self.cue_list.cues.len());

        let original_positions: Vec<usize> = self
            .cue_list
            .cues
            .iter()
            .enumerate()
            .filter_map(|(index, cue)| ordered_ids.contains(&cue.id).then_some(index))
            .collect();
        let removed_before_target = original_positions
            .iter()
            .filter(|index| **index < target_backing_index)
            .count();
        let insert_at = target_backing_index.saturating_sub(removed_before_target);

        for cue in &mut self.cue_list.cues {
            if let CueType::Group { children, .. } = &mut cue.cue_type {
                children.retain(|child_id| !ordered_ids.contains(child_id));
            }
        }

        let mut moving = Vec::new();
        self.cue_list.cues.retain(|cue| {
            if ordered_ids.contains(&cue.id) {
                moving.push(cue.clone());
                false
            } else {
                true
            }
        });
        let at = insert_at.min(self.cue_list.cues.len());
        for (offset, cue) in moving.into_iter().enumerate() {
            self.cue_list.cues.insert(at + offset, cue);
        }
        self.status_msg = format!(
            "Moved {} event(s); cue numbers preserved",
            ordered_ids.len()
        );
    }

    /// Returns the next cue in the visible project sequence, respecting groups.
    /// Group children are followed in their own order. After the last child, the
    /// sequence continues after the parent group (and bubbles through nested groups).
    /// Resolve automatic continuation without accidentally starting the cue
    /// targeted by a Fade event. A target that is immediately after the Fade is
    /// treated as an operated-on track, not as the next independent cue.
    fn next_auto_cue_id(&self, current_id: Uuid) -> Option<Uuid> {
        let next_id = self.next_cue_id(current_id)?;
        let fade_target = self.cue_list.find(current_id).and_then(|cue| {
            if let CueType::Fade { target_id, .. } = &cue.cue_type {
                *target_id
            } else {
                None
            }
        });
        let next_contains_fade_target = fade_target
            .map(|target_id| {
                self.cue_list
                    .find(next_id)
                    .map(|next_cue| {
                        matches!(&next_cue.cue_type, CueType::Group { .. })
                            && self.group_contains(next_id, target_id)
                    })
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if fade_target == Some(next_id) || next_contains_fade_target {
            self.next_cue_id(next_id)
        } else {
            Some(next_id)
        }
    }

    fn next_cue_id(&self, current_id: Uuid) -> Option<Uuid> {
        if let Some(parent_id) = self.parent_group_of(current_id) {
            if let Some(next_sibling) = self.next_group_sibling(parent_id, current_id) {
                return Some(next_sibling);
            }
            return self.next_after_container(parent_id);
        }

        self.next_after_container(current_id)
    }

    fn next_group_sibling(&self, group_id: Uuid, cue_id: Uuid) -> Option<Uuid> {
        let group = self.cue_list.find(group_id)?;
        let CueType::Group { children, .. } = &group.cue_type else {
            return None;
        };
        let position = children.iter().position(|child_id| *child_id == cue_id)?;
        children.get(position + 1).copied()
    }

    /// Find the next item after a root cue or after a completed group. If the
    /// group is nested, continue after it in its parent; otherwise use the next
    /// root-level item. A visited set protects against malformed cyclic groups.
    fn next_after_container(&self, container_id: Uuid) -> Option<Uuid> {
        let mut current_container = container_id;
        let mut visited = HashSet::new();

        loop {
            if !visited.insert(current_container) {
                return None;
            }

            if let Some(parent_id) = self.parent_group_of(current_container) {
                if let Some(next_sibling) = self.next_group_sibling(parent_id, current_container) {
                    return Some(next_sibling);
                }
                current_container = parent_id;
                continue;
            }

            let child_ids: HashSet<Uuid> = self
                .cue_list
                .cues
                .iter()
                .filter_map(|cue| match &cue.cue_type {
                    CueType::Group { children, .. } => Some(children.iter().copied()),
                    _ => None,
                })
                .flatten()
                .collect();
            let root_ids: Vec<Uuid> = self
                .cue_list
                .cues
                .iter()
                .filter(|cue| !child_ids.contains(&cue.id))
                .map(|cue| cue.id)
                .collect();
            let position = root_ids
                .iter()
                .position(|cue_id| *cue_id == current_container)?;
            return root_ids.get(position + 1).copied();
        }
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
            match trig.action {
                PendingAction::ExecuteCue => self.execute_cue_now(trig.cue_id),
                PendingAction::TriggerCue => self.trigger_cue(trig.cue_id),
                PendingAction::CompleteCue => self.complete_cue(trig.cue_id),
            }
        }
    }

    // ── Transport actions ────────────────────────────────────────────────────

    fn stop_selected(&mut self) {
        let id = match self.selected {
            Some(id) => id,
            None => return,
        };
        self.fade_started_at.remove(&id);
        self.fade_managed_targets.remove(&id);
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
        if self.is_managed_group_child(id) {
            self.schedule_cue_completion(id, 0.0);
        }
        self.status_msg = "⏹ Stopped".to_string();
    }

    fn stop_all(&mut self) {
        self.pending_triggers.clear();
        self.active_playlists.clear();
        self.active_all_cues.clear();
        self.fade_started_at.clear();
        self.fade_managed_targets.clear();
        let fade_secs = self.settings.stop_all_fade_secs.max(0.0);
        if fade_secs > 0.0 {
            for cue in &mut self.cue_list.cues {
                let can_fade = matches!(
                    &cue.state,
                    &CueState::Playing | &CueState::FadingIn | &CueState::FadingOut
                ) && matches!(&cue.cue_type, CueType::Audio);
                if can_fade {
                    self.audio.send(AudioCommand::Fade {
                        id: cue.id,
                        target_volume: 0.0,
                        duration_secs: fade_secs,
                        stop_on_finish: true,
                        curve: FadeCurve::Linear,
                        custom_fade_points: vec![[0.0, 0.0], [1.0, 1.0]],
                    });
                    cue.state = CueState::FadingOut;
                } else {
                    if matches!(&cue.cue_type, CueType::Audio) {
                        self.audio.send(AudioCommand::Stop(cue.id));
                    }
                    cue.state = CueState::Stopped;
                    cue.position_secs = 0.0;
                }
            }
            self.status_msg = format!("Stopping all cues with {:.1}s fade", fade_secs);
        } else {
            self.cue_list.stop_all();
            self.audio.send(AudioCommand::StopAll);
            self.status_msg = "⏹ All stopped".to_string();
        }
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
        let Some(id) = self.selected else {
            return;
        };
        let Some(snapshot) = self.cue_list.find(id).cloned() else {
            return;
        };

        match snapshot.state {
            CueState::Paused => {
                self.audio.send(AudioCommand::Resume(id));
                if let Some(cue) = self.cue_list.find_mut(id) {
                    cue.state = CueState::Playing;
                }
                self.report.record(
                    &snapshot.number,
                    &snapshot.name,
                    snapshot.position_secs,
                    "Resume",
                );
                self.status_msg = format!("▶ Resumed: {}", snapshot.name);
            }
            CueState::Stopped | CueState::Error(_) => {
                // Resume Selected is also a convenient Play Selected command
                // when the selected cue has not been started yet.
                self.trigger_cue(id);
            }
            CueState::Waiting
            | CueState::Loading
            | CueState::Playing
            | CueState::FadingIn
            | CueState::FadingOut => {
                // Already scheduled or running: avoid restarting the cue.
            }
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
        let final_path = if self.project_path.is_some() && !self.project_needs_save_as {
            self.assets
                .import_file(&path)
                .unwrap_or_else(|_| path.clone())
        } else {
            path.clone()
        };

        let mut cue = Cue::new_audio(final_path.clone());
        cue.audio_channels = get_channel_count(&final_path);
        if cue.audio_channels > 0 {
            let default_route_channels = self.audio.default_route_output_channels(None);
            cue.io.ensure_channel_routes_with_defaults(
                cue.audio_channels,
                self.audio.output_channels,
                &default_route_channels,
            );
        }
        let dur = get_duration(&final_path);
        if dur > 0.0 {
            cue.duration_secs = dur;
            cue.loop_region.end_secs = dur;
        }
        cue.waveform = generate_waveform(&final_path, 512);

        let name = cue.name.clone();
        let id = self.insert_cue_after_selected(cue);
        self.select_only(id);
        self.status_msg = format!("Added audio cue: {name}");
    }

    /// Insert a newly-created cue immediately after the selected row in the
    /// same sibling sequence. Group children are ordered by their parent's
    /// `children` list; root cues are ordered by the flat project vector.
    fn insert_cue_after_selected(&mut self, cue: Cue) -> Uuid {
        let id = cue.id;
        let selected_id = self.selected;

        // When the Group toolbar button wraps selected rows, replace those rows
        // in their current sibling list with the new group. This avoids leaving
        // the same cue both inside the new group and beside it in the old one.
        let wrapped_children = match &cue.cue_type {
            CueType::Group { children, .. } if !children.is_empty() => children.clone(),
            _ => Vec::new(),
        };
        if !wrapped_children.is_empty() {
            let first_child = selected_id
                .filter(|selected| wrapped_children.contains(selected))
                .unwrap_or(wrapped_children[0]);
            let parent_id = self.parent_group_of(first_child);
            let sibling_insert_at = parent_id.and_then(|parent_id| {
                self.cue_list.find(parent_id).and_then(|parent| {
                    if let CueType::Group { children, .. } = &parent.cue_type {
                        children.iter().position(|child| *child == first_child)
                    } else {
                        None
                    }
                })
            });
            let root_insert_at = self
                .cue_list
                .cues
                .iter()
                .position(|existing| existing.id == first_child);

            self.cue_list.add(cue);
            for existing in &mut self.cue_list.cues {
                if existing.id != id {
                    if let CueType::Group { children, .. } = &mut existing.cue_type {
                        children.retain(|child_id| !wrapped_children.contains(child_id));
                    }
                }
            }
            if let Some(parent_id) = parent_id {
                if let Some(parent) = self.cue_list.find_mut(parent_id) {
                    if let CueType::Group { children, .. } = &mut parent.cue_type {
                        let at = sibling_insert_at
                            .unwrap_or(children.len())
                            .min(children.len());
                        children.insert(at, id);
                    }
                }
            } else if let Some(at) = root_insert_at {
                if let Some(new_group) = self.cue_list.cues.pop() {
                    self.cue_list
                        .cues
                        .insert(at.min(self.cue_list.cues.len()), new_group);
                }
            }
            return id;
        }

        let parent_id = selected_id.and_then(|selected| self.parent_group_of(selected));
        self.cue_list.add(cue);

        if let Some(selected_id) = selected_id {
            if let Some(parent_id) = parent_id {
                if let Some(parent) = self.cue_list.find_mut(parent_id) {
                    if let CueType::Group { children, .. } = &mut parent.cue_type {
                        let insert_at = children
                            .iter()
                            .position(|child_id| *child_id == selected_id)
                            .map(|index| index + 1)
                            .unwrap_or(children.len());
                        children.insert(insert_at, id);
                    }
                }
            } else if let Some(insert_at) = self
                .cue_list
                .cues
                .iter()
                .position(|existing| existing.id == selected_id)
                .map(|index| index + 1)
            {
                // CueList::add assigned the stable cue number. Move only the
                // just-added row; never renumber any existing cue.
                if let Some(new_cue) = self.cue_list.cues.pop() {
                    self.cue_list
                        .cues
                        .insert(insert_at.min(self.cue_list.cues.len()), new_cue);
                }
            }
        }
        id
    }

    fn add_typed_cue(&mut self, cue_type: CueType) {
        let is_midi = matches!(&cue_type, CueType::Midi { .. });
        let mut cue = Cue::new_typed(cue_type);
        let default_target = self.selected.filter(|selected_id| {
            self.cue_list
                .find(*selected_id)
                .map(|selected_cue| {
                    matches!(
                        &selected_cue.cue_type,
                        CueType::Audio | CueType::Group { .. }
                    )
                })
                .unwrap_or(false)
        });
        match &mut cue.cue_type {
            CueType::Play { target_id }
            | CueType::Pause { target_id }
            | CueType::Stop { target_id }
            | CueType::Fade { target_id, .. } => {
                if target_id.is_none() {
                    *target_id = default_target;
                }
            }
            _ => {}
        }
        let label = cue.name.clone();
        let id = self.insert_cue_after_selected(cue);
        self.select_only(id);
        if is_midi {
            self.active_panel = Panel::Midi;
        } else {
            self.active_panel = Panel::Inspector;
        }
        self.status_msg = format!("Added {label} event");
    }

    fn delete_cue(&mut self, id: Uuid) {
        self.fade_started_at.remove(&id);
        self.fade_managed_targets.remove(&id);
        self.audio.send(AudioCommand::Stop(id));
        self.selected_ids.remove(&id);
        if self.selected == Some(id) {
            self.selected = self.selected_ids.iter().next().copied();
        }
        if self.selection_anchor == Some(id) {
            self.selection_anchor = self.selected;
        }
        // Убираем id из всех групп
        for c in &mut self.cue_list.cues {
            if let CueType::Group { children, .. } = &mut c.cue_type {
                children.retain(|x| *x != id);
            }
        }
        self.cue_list.remove(id);
        self.expanded_groups.remove(&id);
        self.active_playlists
            .retain(|run| run.group_id != id && !run.children.contains(&id));
        self.active_all_cues
            .retain(|run| run.group_id != id && !run.remaining.contains(&id));
    }

    fn select_only(&mut self, id: Uuid) {
        self.selected = Some(id);
        self.selected_ids.clear();
        self.selected_ids.insert(id);
        self.selection_anchor = Some(id);
    }

    fn visible_selection_position(&self, visible: &[Uuid]) -> Option<usize> {
        let mut current = self.selected?;
        let mut visited = HashSet::new();
        loop {
            if let Some(position) = visible.iter().position(|item| *item == current) {
                return Some(position);
            }
            if !visited.insert(current) {
                return None;
            }
            current = self.parent_group_of(current)?;
        }
    }

    fn select_next(&mut self) {
        let visible = visible_cue_ids(&self.cue_list.cues, &self.expanded_groups);
        if visible.is_empty() {
            return;
        }
        let next = match self.visible_selection_position(&visible) {
            Some(position) => visible[(position + 1).min(visible.len() - 1)],
            None => visible[0],
        };
        self.select_only(next);
    }

    fn select_prev(&mut self) {
        let visible = visible_cue_ids(&self.cue_list.cues, &self.expanded_groups);
        if visible.is_empty() {
            return;
        }
        let previous = match self.visible_selection_position(&visible) {
            Some(position) => visible[position.saturating_sub(1)],
            None => visible[0],
        };
        self.select_only(previous);
    }

    // ── Сохранение и загрузка проекта ────────────────────────────────────────

    fn save_project(&mut self) {
        info!(
            "Save requested: path={:?}, project_name={:?}, save_as_required={}",
            self.project_path, self.project.settings.name, self.project_needs_save_as
        );
        // Only a real title change requires Save As. Reconcile the flag with
        // the opened filename so a stale flag can never turn ordinary Ctrl+S
        // into Save As. Editing cues/settings must always save in place.
        let renamed_from_opened_file = self.project_path.as_deref().is_some_and(|opened_path| {
            project_name_from_path(opened_path) != self.project.settings.name
        });
        self.project_needs_save_as = renamed_from_opened_file;
        if renamed_from_opened_file {
            self.status_msg =
                "Project name changed — choose a new project file with Save As".to_string();
            self.save_as_project();
            return;
        }

        let is_first_save = self.project_path.is_none();
        if is_first_save {
            let default_name = format!("{}.clabs", self.project.settings.name);
            let path = FileDialog::new()
                .add_filter("CueLabs Project", &["clabs"])
                .set_file_name(&default_name)
                .save_file();
            match path {
                Some(path) => {
                    let path = ensure_project_extension(path);
                    self.project.settings.name = project_name_from_path(&path);
                    self.assets.set_project_path(path.clone());
                    self.project_path = Some(path);
                }
                None => return,
            }
        }

        let path = self.project_path.clone().unwrap();

        // Move files that were added before the first Save into the project audio folder.
        let pending_assets: Vec<(Uuid, PathBuf)> = self
            .cue_list
            .cues
            .iter()
            .filter(|cue| matches!(&cue.cue_type, CueType::Audio))
            .filter(|cue| cue.file_path.is_file())
            .map(|cue| (cue.id, cue.file_path.clone()))
            .collect();
        for (cue_id, source_path) in pending_assets {
            match self.assets.import_file(&source_path) {
                Ok(asset_path) => {
                    if asset_path != source_path {
                        if let Some(cue) = self.cue_list.find_mut(cue_id) {
                            cue.file_path = asset_path;
                        }
                    }
                }
                Err(error) => {
                    self.status_msg = format!("Asset copy error: {error}");
                    error!("Could not copy project asset: {error}");
                    return;
                }
            }
        }

        self.project.settings.name = project_name_from_path(&path);
        self.report.project_name = self.project.settings.name.clone();
        self.project.cue_list = self.cue_list.clone();

        match self.project.save(&path) {
            Ok(_) => {
                self.status_msg = format!("💾 Saved: {}", path.display());
                if is_first_save {
                    self.last_periodic_backup_at = Instant::now();
                }
                self.create_project_backup("save");
            }
            Err(e) => {
                self.status_msg = format!("Save error: {e}");
                error!("{e}");
            }
        }
    }

    /// Save the current project under a new filename. Media references are
    /// preserved; use Save as Bundle to copy media into a portable folder.
    fn save_as_project(&mut self) {
        let default_name = format!("{}.clabs", self.project.settings.name);

        let Some(path) = FileDialog::new()
            .add_filter("CueLabs Project", &["clabs"])
            .set_file_name(&default_name)
            .save_file()
        else {
            return;
        };
        let path = ensure_project_extension(path);

        if self.project_needs_save_as
            && self
                .project_path
                .as_deref()
                .is_some_and(|opened_path| paths_refer_to_same_location(opened_path, &path))
        {
            self.status_msg =
                "The original project file is protected. Choose a different name or location."
                    .to_string();
            return;
        }

        let mut project = self.project.clone();
        project.settings.name = project_name_from_path(&path);
        project.cue_list = self.cue_list.clone();

        match project.save(&path) {
            Ok(()) => {
                self.project = project;
                self.project_path = Some(path.clone());
                self.assets.set_project_path(path.clone());
                self.report.project_name = self.project.settings.name.clone();
                self.project_needs_save_as = false;
                self.last_periodic_backup_at = Instant::now();
                self.status_msg = format!("💾 Saved as: {}", path.display());
                self.create_project_backup("save-as");
            }
            Err(error) => {
                self.status_msg = format!("Save As error: {error}");
                error!("Could not save project copy: {error}");
            }
        }
    }

    /// Create a portable bundle folder:
    /// <parent>/<project>/<project>.clabs + audio/ with relative media paths.
    fn save_as_bundle(&mut self) {
        let default_name = self.project.settings.name.clone();
        let default_file_name = format!("{default_name}.clabs");

        let Some(selected_path) = FileDialog::new()
            .add_filter("CueLabs Project Bundle", &["clabs"])
            .set_file_name(&default_file_name)
            .save_file()
        else {
            return;
        };

        // The selected filename determines both the bundle folder and project
        // filename. The selected path itself is a naming/location hint; the
        // actual project file is created one level deeper.
        let selected_path = ensure_project_extension(selected_path);
        let bundle_name = project_name_from_path(&selected_path);
        let bundle_parent = selected_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let bundle_dir = bundle_parent.join(&bundle_name);
        let bundle_project_path = bundle_dir.join(format!("{bundle_name}.clabs"));
        let audio_dir = bundle_dir.join("audio");

        if bundle_dir.exists() && !bundle_dir.is_dir() {
            self.status_msg = format!(
                "Bundle error: destination exists and is not a folder: {}",
                bundle_dir.display()
            );
            return;
        }

        if let Err(error) = std::fs::create_dir_all(&audio_dir) {
            self.status_msg = format!("Bundle error: {error}");
            error!("Could not create bundle directory: {error}");
            return;
        }

        let mut bundle_assets = AssetManager::new();
        bundle_assets.set_project_path(bundle_project_path.clone());

        let mut bundle_cues = self.cue_list.clone();
        let mut copied_files: HashMap<PathBuf, PathBuf> = HashMap::new();
        let mut copied_count = 0usize;
        let mut missing_count = 0usize;

        for cue in &mut bundle_cues.cues {
            if !matches!(&cue.cue_type, CueType::Audio) {
                continue;
            }

            let source_path = cue.file_path.clone();
            if source_path.as_os_str().is_empty() {
                cue.file_path = audio_dir.join("missing").join("unknown-media");
                missing_count += 1;
                continue;
            }

            if source_path.is_file() {
                let source_key = source_path
                    .canonicalize()
                    .unwrap_or_else(|_| source_path.clone());

                let destination = if let Some(existing) = copied_files.get(&source_key) {
                    existing.clone()
                } else {
                    match bundle_assets.import_file(&source_path) {
                        Ok(destination) => {
                            copied_files.insert(source_key, destination.clone());
                            copied_count += 1;
                            destination
                        }
                        Err(error) => {
                            self.status_msg = format!("Bundle audio copy error: {error}");
                            error!("Could not copy bundle audio file: {error}");
                            return;
                        }
                    }
                };
                cue.file_path = destination;
            } else {
                // Keep an expected relative location for missing media. This
                // allows the project to open elsewhere and show the missing-file
                // warning without retaining an absolute path from this computer.
                let file_name = source_path
                    .file_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("unknown-media"));
                cue.file_path = audio_dir.join("missing").join(file_name);
                missing_count += 1;
            }
        }

        let mut project = self.project.clone();
        project.settings.name = bundle_name.clone();
        project.cue_list = bundle_cues.clone();

        match project.save(&bundle_project_path) {
            Ok(()) => {
                self.cue_list = bundle_cues;
                self.project = project;
                self.project_path = Some(bundle_project_path.clone());
                self.assets.set_project_path(bundle_project_path);
                self.report.project_name = self.project.settings.name.clone();
                self.project_needs_save_as = false;
                self.last_periodic_backup_at = Instant::now();
                self.status_msg = format!(
                    "📦 Bundle saved: {} | audio files: {} | missing: {}",
                    bundle_dir.display(),
                    copied_count,
                    missing_count
                );
                self.create_project_backup("bundle-save");
            }
            Err(error) => {
                self.status_msg = format!("Bundle save error: {error}");
                error!("Could not save project bundle: {error}");
            }
        }
    }

    fn rename_project(&mut self, requested_name: String) {
        let Some(new_name) = sanitize_project_name(&requested_name) else {
            self.status_msg =
                "Project name cannot be empty or contain only invalid characters".to_string();
            return;
        };

        let old_name = self.project.settings.name.clone();
        if new_name == old_name || new_name.eq_ignore_ascii_case(&old_name) {
            self.project_name_edit = None;
            return;
        }

        // Keep the opened file and its containing Bundle untouched. The new
        // name becomes the in-memory project name; the next Save operation
        // will open Save As and write a separate project file.
        self.project.settings.name = new_name.clone();
        self.project.cue_list = self.cue_list.clone();
        self.report.project_name = new_name.clone();
        self.project_needs_save_as = self.project_path.is_some();
        self.status_msg = if self.project_needs_save_as {
            format!("Project renamed to {new_name}. Save to create a new project file.")
        } else {
            format!("Project renamed: {new_name}")
        };
    }

    fn project_backup_fingerprint(&self) -> Option<u64> {
        let mut snapshot = self.project.clone();
        snapshot.cue_list = self.cue_list.clone();
        let serialized = serde_json::to_vec(&snapshot).ok()?;
        let mut hasher = DefaultHasher::new();
        serialized.hash(&mut hasher);
        Some(hasher.finish())
    }

    fn create_project_backup(&mut self, reason: &str) {
        let Some(project_path) = self.project_path.clone() else {
            return;
        };

        // Do not create nested backup/backup folders when a snapshot itself
        // has been opened for recovery.
        let is_backup_project = project_path
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("backup"));
        if is_backup_project {
            return;
        }

        let project_dir = project_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let backup_dir = project_dir.join("backup");

        if let Err(error) = std::fs::create_dir_all(&backup_dir) {
            self.status_msg = format!("Backup error: {error}");
            warn!("Could not create project backup directory: {error}");
            if reason == "periodic" {
                self.last_periodic_backup_at = Instant::now();
            }
            return;
        }

        self.project.cue_list = self.cue_list.clone();
        let safe_name = sanitize_project_name(&self.project.settings.name)
            .unwrap_or_else(|| "Project".to_string());
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S_%f");
        let backup_path = backup_dir.join(format!("{safe_name}__{timestamp}.clabs"));

        match self.project.save_backup(&backup_path, &project_path) {
            Ok(()) => {
                self.last_backup_fingerprint = self.project_backup_fingerprint();
                if reason == "periodic" {
                    self.last_periodic_backup_at = Instant::now();
                }
                info!("Created {reason} backup: {:?}", backup_path);

                // The retention limit applies to the whole backup folder,
                // not just the current project-name prefix. This also handles
                // snapshots created before/after a project rename.
                let mut backups: Vec<PathBuf> = match std::fs::read_dir(&backup_dir) {
                    Ok(entries) => entries
                        .filter_map(|entry| entry.ok())
                        .map(|entry| entry.path())
                        .filter(|path| {
                            path.is_file()
                                && path
                                    .extension()
                                    .and_then(|extension| extension.to_str())
                                    .is_some_and(|extension| {
                                        extension.eq_ignore_ascii_case("clabs")
                                    })
                        })
                        .collect(),
                    Err(error) => {
                        warn!("Could not list project backups: {error}");
                        Vec::new()
                    }
                };
                backups.sort_by_key(|path| {
                    std::fs::metadata(path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|duration| duration.as_nanos())
                        .unwrap_or(0)
                });

                while backups.len() > 10 {
                    let oldest = backups.remove(0);
                    if let Err(error) = std::fs::remove_file(&oldest) {
                        warn!("Could not remove old project backup {:?}: {error}", oldest);
                    }
                }
            }
            Err(error) => {
                self.status_msg = format!("Backup error: {error}");
                warn!("Could not create project backup: {error}");
                if reason == "periodic" {
                    self.last_periodic_backup_at = Instant::now();
                }
            }
        }
    }

    fn poll_periodic_backup(&mut self) {
        let interval_minutes = self.settings.backup_interval_minutes;
        if interval_minutes == 0
            || self.project_path.is_none()
            || self.project_needs_save_as
            || self.last_periodic_backup_at.elapsed()
                < Duration::from_secs(interval_minutes as u64 * 60)
        {
            return;
        }

        // A manual Save creates its own snapshot, but must not restart the
        // periodic clock. This clock is independent so timed backups still
        // happen on schedule even when the user saves frequently.
        self.last_periodic_backup_at = Instant::now();
        let current_fingerprint = self.project_backup_fingerprint();
        if current_fingerprint.is_some() && current_fingerprint == self.last_backup_fingerprint {
            info!("Skipping periodic backup: project has not changed since the last snapshot");
            return;
        }

        self.create_project_backup("periodic");
    }

    fn poll_audio_file_metadata(&mut self) {
        if self.last_media_scan_at.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_media_scan_at = Instant::now();

        for cue in &mut self.cue_list.cues {
            if !matches!(&cue.cue_type, CueType::Audio) {
                continue;
            }

            if !cue.file_path.is_file() {
                // Clear stale runtime metadata when media disappears. The
                // next scan will rebuild it as soon as the file is restored.
                cue.duration_secs = 0.0;
                cue.waveform.clear();
                continue;
            }

            if cue.duration_secs > 0.0 && !cue.waveform.is_empty() {
                continue;
            }

            let duration = get_duration(&cue.file_path);
            if duration <= 0.0 {
                continue;
            }
            let waveform = generate_waveform(&cue.file_path, 512);
            if waveform.is_empty() {
                continue;
            }
            let channels = get_channel_count(&cue.file_path);

            cue.duration_secs = duration;
            cue.waveform = waveform;
            if cue.loop_region.end_secs <= 0.0 {
                cue.loop_region.end_secs = duration;
            } else {
                cue.loop_region.end_secs = cue.loop_region.end_secs.min(duration);
            }
            cue.start_secs = cue.start_secs.min(duration);
            if let Some(end_secs) = cue.end_secs.as_mut() {
                *end_secs = (*end_secs).min(duration);
            }

            if channels > 0 {
                cue.audio_channels = channels;
                let output_count = self
                    .audio
                    .output_channels_for_device(cue.io.output_device.as_deref());
                let default_route_channels = self
                    .audio
                    .default_route_output_channels(cue.io.output_device.as_deref());
                cue.io.ensure_channel_routes_with_defaults(
                    channels,
                    output_count,
                    &default_route_channels,
                );
            }

            info!(
                "Recovered audio metadata for cue {} from {:?}",
                cue.id, cue.file_path
            );
        }
    }

    fn set_audio_cue_file(&mut self, cue_id: Uuid, source_path: PathBuf) {
        if !source_path.is_file() {
            self.status_msg = format!("Audio file does not exist: {}", source_path.display());
            return;
        }

        let duration = get_duration(&source_path);
        let waveform = generate_waveform(&source_path, 512);
        let channels = get_channel_count(&source_path);
        if duration <= 0.0 || waveform.is_empty() {
            self.status_msg = format!("Could not decode audio file: {}", source_path.display());
            return;
        }

        let is_backup_project = self
            .project_path
            .as_deref()
            .and_then(|path| path.parent())
            .and_then(|parent| parent.file_name())
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("backup"));
        let destination =
            if self.project_path.is_some() && !is_backup_project && !self.project_needs_save_as {
                match self.assets.import_file(&source_path) {
                    Ok(path) => path,
                    Err(error) => {
                        self.status_msg = format!("Could not import audio file: {error}");
                        error!("Could not import replacement audio file: {error}");
                        return;
                    }
                }
            } else {
                source_path
            };

        if let Some(cue) = self.cue_list.find_mut(cue_id) {
            if !matches!(&cue.cue_type, CueType::Audio) {
                return;
            }

            if cue.is_active() {
                self.audio.send(AudioCommand::Stop(cue_id));
            }
            let previous_duration = cue.duration_secs;
            cue.state = CueState::Stopped;
            cue.position_secs = 0.0;
            cue.file_path = destination.clone();
            cue.duration_secs = duration;
            cue.waveform = waveform;
            if channels > 0 {
                cue.audio_channels = channels;
                let output_count = self
                    .audio
                    .output_channels_for_device(cue.io.output_device.as_deref());
                let default_route_channels = self
                    .audio
                    .default_route_output_channels(cue.io.output_device.as_deref());
                cue.io.ensure_channel_routes_with_defaults(
                    channels,
                    output_count,
                    &default_route_channels,
                );
            }

            if cue.loop_region.end_secs <= 0.0
                || previous_duration <= 0.0
                || (cue.loop_region.end_secs - previous_duration).abs() < 0.05
            {
                cue.loop_region.end_secs = duration;
            } else {
                cue.loop_region.end_secs = cue.loop_region.end_secs.min(duration);
            }
            cue.start_secs = cue.start_secs.min(duration);
            if let Some(end_secs) = cue.end_secs.as_mut() {
                *end_secs = (*end_secs).min(duration);
            }
        }

        self.status_msg = format!(
            "Audio file updated: {} · {}",
            destination.display(),
            crate::ui::cue_table::format_duration(duration)
        );
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
                            let detected_channels = get_channel_count(&cue.file_path);
                            if detected_channels > 0 {
                                cue.audio_channels = detected_channels;
                            }
                            if cue.audio_channels > 0 {
                                let output_count = self
                                    .audio
                                    .output_channels_for_device(cue.io.output_device.as_deref());
                                let default_route_channels = self
                                    .audio
                                    .default_route_output_channels(cue.io.output_device.as_deref());
                                cue.io.ensure_channel_routes_with_defaults(
                                    cue.audio_channels,
                                    output_count,
                                    &default_route_channels,
                                );
                            }
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
                    self.project = proj;
                    let is_backup_project = path
                        .parent()
                        .and_then(|parent| parent.file_name())
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.eq_ignore_ascii_case("backup"));
                    if !is_backup_project {
                        self.assets.set_project_path(path.clone());
                        self.project_path = Some(path.clone());
                        self.project.settings.name = project_name_from_path(&path);
                    } else {
                        // Opening a backup is a recovery operation. Keep the
                        // snapshot untouched and let the user Save As a normal
                        // project when ready.
                        self.assets = AssetManager::new();
                        self.project_path = None;
                    }
                    self.project_needs_save_as = false;
                    self.last_periodic_backup_at = Instant::now();
                    self.last_backup_fingerprint = self.project_backup_fingerprint();
                    self.last_media_scan_at = Instant::now();
                    self.report = SessionReport::new(&self.project.settings.name);
                    self.selected = None;
                    self.selected_ids.clear();
                    self.selection_anchor = None;
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
        self.project_needs_save_as = false;
        self.assets = AssetManager::new();
        self.report = SessionReport::new("Untitled Project");
        self.selected = None;
        self.selected_ids.clear();
        self.selection_anchor = None;
        self.last_periodic_backup_at = Instant::now();
        self.last_backup_fingerprint = None;
        self.last_media_scan_at = Instant::now();
        self.status_msg = "New project".to_string();
    }

    // ── Опрос аудио, MIDI и OSC ──────────────────────────────────────────────

    fn poll_audio(&mut self) {
        let mut finished_cues = Vec::new();

        for update in self.audio.drain_position_updates() {
            if let Some(cue) = self.cue_list.find_mut(update.id) {
                let was_active = cue.is_active();
                cue.state = update.state.clone();
                cue.position_secs = update.position_secs;
                if update.state == CueState::Stopped {
                    cue.position_secs = 0.0;
                    if was_active {
                        finished_cues.push(update.id);
                    }
                }
            }
        }

        for cue_id in finished_cues {
            self.schedule_cue_completion(cue_id, 0.0);
        }

        let now = Instant::now();
        let active_fades: Vec<(Uuid, Instant)> = self
            .fade_started_at
            .iter()
            .map(|(id, started)| (*id, *started))
            .collect();
        for (fade_id, started_at) in active_fades {
            if let Some(fade_cue) = self.cue_list.find_mut(fade_id) {
                if matches!(&fade_cue.cue_type, CueType::Fade { .. })
                    && matches!(
                        &fade_cue.state,
                        CueState::FadingIn | CueState::FadingOut | CueState::Waiting
                    )
                {
                    let elapsed = now.duration_since(started_at).as_secs_f64();
                    let duration = fade_cue.effective_duration().max(0.0);
                    fade_cue.position_secs = elapsed.min(duration);
                    if elapsed >= duration && fade_cue.post_wait_secs > 0.0 {
                        fade_cue.state = CueState::Waiting;
                    }
                }
            }
        }
    }

    fn poll_midi(&mut self) {
        for msg in self.midi.drain() {
            // Settings is modal for Cue List actions. Only an explicit MIDI Learn
            // capture is allowed to consume messages while the window is open.
            if self.settings_open {
                if self.midi_learn_action.is_some() {
                    self.handle_midi_control_message(&msg);
                }
                self.midi_cc_pressed.clear();
                continue;
            }
            if self.handle_midi_control_message(&msg) {
                continue;
            }

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

    /// Handles MIDI learn and persistent command bindings. Channels are stored
    /// 1-based for display; incoming MIDI messages use the MIDI-standard 0-based value.
    fn handle_midi_control_message(&mut self, message: &crate::midi::input::MidiMessage) -> bool {
        use crate::midi::input::MidiMessage;

        // Consume both press and release for an already mapped CC. In particular,
        // this prevents a button mapped to CC 7 from also changing master volume
        // when its release value (0) arrives.
        if let MidiMessage::CC { channel, cc, value } = message {
            let key = (*channel, *cc);
            let has_binding = self.settings.midi_controls.iter().any(|binding| {
                binding.channel == *channel + 1
                    && binding.kind == MidiControlKind::ControlChange
                    && binding.number == *cc
            });
            if *value == 0 {
                self.midi_cc_pressed.remove(&key);
                if has_binding && self.midi_learn_action.is_none() {
                    return true;
                }
            } else if !self.midi_cc_pressed.insert(key)
                && has_binding
                && self.midi_learn_action.is_none()
            {
                return true;
            }
        }

        let detected = match message {
            MidiMessage::NoteOn { channel, note, .. } => {
                Some((*channel + 1, MidiControlKind::NoteOn, *note))
            }
            // Trigger a CC binding once on the transition from released (0) to
            // pressed (any non-zero value). This works for controllers that send
            // either 1/0 or 127/0 and avoids firing GO again on button release.
            MidiMessage::CC { channel, cc, value } if *value > 0 => {
                Some((*channel + 1, MidiControlKind::ControlChange, *cc))
            }
            _ => None,
        };

        if let Some(action) = self.midi_learn_action.take() {
            if let Some((channel, kind, number)) = detected {
                self.settings.midi_controls.retain(|binding| {
                    !(binding.channel == channel
                        && binding.kind == kind
                        && binding.number == number)
                });
                self.settings.midi_controls.push(MidiControlBinding {
                    channel,
                    kind,
                    number,
                    action: action.clone(),
                });
                self.save_app_settings();
                self.status_msg = format!(
                    "MIDI control learned: {} → {}",
                    MidiControlBinding {
                        channel,
                        kind,
                        number,
                        action: action.clone()
                    }
                    .label(),
                    action.label()
                );
                return true;
            }
            self.midi_learn_action = Some(action);
            return true;
        }

        let Some((channel, kind, number)) = detected else {
            return false;
        };
        let action = self
            .settings
            .midi_controls
            .iter()
            .find(|binding| {
                binding.channel == channel && binding.kind == kind && binding.number == number
            })
            .map(|binding| binding.action.clone());
        if let Some(action) = action {
            self.dispatch_app_action(action);
            true
        } else {
            false
        }
    }

    fn poll_osc(&mut self) {
        let events = match &self.osc {
            Some(osc) => osc.drain(),
            None => return,
        };
        if self.settings_open {
            // Drain but do not dispatch remote Cue List actions behind the modal.
            return;
        }
        for ev in events {
            match ev.addr.as_str() {
                "/go" | "/cue/go" => self.dispatch_app_action(AppAction::Go),
                "/stop" | "/stop_all" | "/cue/stop" => self.dispatch_app_action(AppAction::StopAll),
                "/stop_selected" | "/cue/stop_selected" => {
                    self.dispatch_app_action(AppAction::StopSelected)
                }
                "/pause" | "/pause_all" | "/cue/pause" => {
                    self.dispatch_app_action(AppAction::PauseAll)
                }
                "/pause_selected" | "/cue/pause_selected" => {
                    self.dispatch_app_action(AppAction::PauseSelected)
                }
                "/resume" | "/resume_all" | "/cue/resume" => {
                    self.dispatch_app_action(AppAction::ResumeAll)
                }
                "/resume_selected" | "/cue/resume_selected" => {
                    self.dispatch_app_action(AppAction::ResumeSelected)
                }
                "/next" | "/select_next" | "/cue/next" => {
                    self.dispatch_app_action(AppAction::SelectNext)
                }
                "/previous" | "/select_previous" | "/cue/previous" => {
                    self.dispatch_app_action(AppAction::SelectPrev)
                }
                "/delete_selected" | "/cue/delete_selected" => {
                    self.dispatch_app_action(AppAction::DeleteSelected)
                }
                "/rename_selected" | "/cue/rename_selected" => {
                    self.dispatch_app_action(AppAction::RenameSelected)
                }
                "/move_up" | "/cue/move_up" => self.dispatch_app_action(AppAction::MoveUp),
                "/move_down" | "/cue/move_down" => self.dispatch_app_action(AppAction::MoveDown),
                "/new_project" | "/cue/new_project" => {
                    self.dispatch_app_action(AppAction::NewProject)
                }
                "/open_project" | "/cue/open_project" => {
                    self.dispatch_app_action(AppAction::OpenProject)
                }
                "/save_project" | "/cue/save_project" => {
                    self.dispatch_app_action(AppAction::SaveProject)
                }
                "/add_file" | "/cue/add_file" => self.dispatch_app_action(AppAction::AddFile),
                _ => {}
            }
        }
    }

    // ── Горячие клавиши и Drag-and-drop ──────────────────────────────────────

    fn handle_keyboard(&mut self, ctx: &Context) {
        // The settings window is modal. Keep its explicit hotkey capture active,
        // but never dispatch normal transport or project shortcuts behind it.
        if self.settings_open && self.hotkey_capture.is_none() {
            return;
        }

        let key_events: Vec<(Key, egui::Modifiers)> = ctx.input(|input| {
            input
                .events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => Some((*key, *modifiers)),
                    _ => None,
                })
                .collect()
        });

        // Capture mode has priority over normal application shortcuts.
        if let Some(action) = self.hotkey_capture.clone() {
            if let Some((key, modifiers)) = key_events.first().copied() {
                if key == Key::Escape {
                    self.hotkey_capture = None;
                } else {
                    let binding = Self::format_hotkey(key, modifiers);
                    self.settings.hotkeys.bindings.insert(action, binding);
                    self.hotkey_capture = None;
                    self.save_app_settings();
                    self.status_msg = "Hotkey updated".to_string();
                }
            }
            return;
        }

        // Project save shortcuts are global, including while a text editor has
        // focus. This matches normal desktop-app behaviour: Ctrl+S saves the
        // project, Ctrl+Shift+S always opens Save As, and Ctrl+Alt+S bundles.
        if key_events.iter().any(|(key, modifiers)| {
            *key == Key::S && modifiers.ctrl && modifiers.shift && !modifiers.alt
        }) {
            self.save_as_project();
            return;
        }
        if key_events.iter().any(|(key, modifiers)| {
            *key == Key::S && modifiers.ctrl && modifiers.alt && !modifiers.shift
        }) {
            self.save_as_bundle();
            return;
        }
        let save_binding = self.settings.hotkeys.bindings.get(&AppAction::SaveProject);
        if save_binding.is_some_and(|binding| {
            key_events.iter().any(|(key, modifiers)| {
                Self::format_hotkey(*key, *modifiers).eq_ignore_ascii_case(binding)
            })
        }) {
            self.save_project();
            return;
        }

        // Other shortcuts should not steal keystrokes from text fields/editors.
        if ctx.memory(|memory| memory.focused().is_some()) {
            return;
        }

        if let Some(selected_id) = self.selected {
            let selected_is_group = self
                .cue_list
                .find(selected_id)
                .map(|cue| matches!(&cue.cue_type, CueType::Group { .. }))
                .unwrap_or(false);
            if selected_is_group && key_events.iter().any(|(key, _)| *key == Key::ArrowRight) {
                self.expanded_groups.insert(selected_id);
                return;
            }
            if key_events.iter().any(|(key, _)| *key == Key::ArrowLeft) {
                if selected_is_group && self.expanded_groups.remove(&selected_id) {
                    return;
                }
                if let Some(parent_id) = self.parent_group_of(selected_id) {
                    self.expanded_groups.remove(&parent_id);
                    self.select_only(parent_id);
                    return;
                }
            }
        }

        for action in AppAction::ALL {
            if *action == AppAction::SaveProject {
                continue;
            }
            let Some(binding) = self.settings.hotkeys.bindings.get(action) else {
                continue;
            };
            if key_events.iter().any(|(key, modifiers)| {
                Self::format_hotkey(*key, *modifiers).eq_ignore_ascii_case(binding)
            }) {
                self.dispatch_app_action((*action).clone());
                break;
            }
        }
    }

    fn format_hotkey(key: Key, modifiers: egui::Modifiers) -> String {
        let mut parts = Vec::new();
        if modifiers.ctrl {
            parts.push("Ctrl".to_string());
        }
        if modifiers.alt {
            parts.push("Alt".to_string());
        }
        if modifiers.shift {
            parts.push("Shift".to_string());
        }
        let key_name = match key {
            Key::ArrowDown => "Down".to_string(),
            Key::ArrowUp => "Up".to_string(),
            Key::ArrowLeft => "Left".to_string(),
            Key::ArrowRight => "Right".to_string(),
            _ => format!("{:?}", key),
        };
        parts.push(key_name);
        parts.join("+")
    }

    fn dispatch_app_action(&mut self, action: AppAction) {
        match action {
            AppAction::Go => self.go(),
            AppAction::StopSelected => self.stop_selected(),
            AppAction::StopAll => self.stop_all(),
            AppAction::PauseSelected => self.pause_selected(),
            AppAction::PauseAll => self.pause_all(),
            AppAction::ResumeSelected => self.resume_selected(),
            AppAction::ResumeAll => self.resume_all(),
            AppAction::SelectNext => self.select_next(),
            AppAction::SelectPrev => self.select_prev(),
            AppAction::NewProject => self.new_project(),
            AppAction::OpenProject => self.open_project(),
            AppAction::SaveProject => self.save_project(),
            AppAction::AddFile => self.add_files(),
            AppAction::DeleteSelected => {
                let ids: Vec<Uuid> = if self.selected_ids.is_empty() {
                    self.selected.into_iter().collect()
                } else {
                    self.selected_ids.iter().copied().collect()
                };
                for id in ids {
                    self.delete_cue(id);
                }
            }
            AppAction::RenameSelected => {
                if let Some(id) = self.selected {
                    if let Some(cue) = self.cue_list.find(id) {
                        self.editing = Some((id, EditField::Name, cue.name.clone()));
                        self.editing_focus_pending = true;
                    }
                }
            }
            AppAction::MoveUp => {
                if let Some(id) = self.selected {
                    self.cue_list.move_up(id);
                }
            }
            AppAction::MoveDown => {
                if let Some(id) = self.selected {
                    self.cue_list.move_down(id);
                }
            }
        }
    }

    fn switch_audio_backend(&mut self, backend: AudioBackend) {
        if self.audio.backend == backend {
            return;
        }

        self.stop_all();
        match AudioEngine::new_with_backend(backend) {
            Ok(mut replacement) => {
                if let Some(saved_device) = self.settings.audio_output_device.as_deref() {
                    if replacement
                        .output_devices
                        .iter()
                        .any(|device| device == saved_device)
                    {
                        replacement.set_output_device(saved_device);
                    }
                }
                replacement.set_output_channel_names(self.settings.output_channel_names.clone());
                replacement.set_default_output_pairs(self.settings.default_output_pairs.clone());
                let backend_devices = replacement.output_devices.clone();
                self.audio = replacement;
                self.settings.audio_backend = backend;
                self.settings.audio_output_device = Some(self.audio.current_device.clone());
                self.settings.output_channels = self.audio.output_channel_settings();
                for cue in &mut self.cue_list.cues {
                    if matches!(&cue.cue_type, CueType::Audio) {
                        if cue
                            .io
                            .output_device
                            .as_ref()
                            .is_some_and(|name| !backend_devices.contains(name))
                        {
                            cue.io.output_device = None;
                        }
                        let output_count = self
                            .audio
                            .output_channels_for_device(cue.io.output_device.as_deref());
                        let default_route_channels = self
                            .audio
                            .default_route_output_channels(cue.io.output_device.as_deref());
                        cue.io.ensure_channel_routes_with_defaults(
                            cue.audio_channels.max(1),
                            output_count,
                            &default_route_channels,
                        );
                    }
                }
                self.save_app_settings();
                self.status_msg = format!("Audio backend switched to {}", backend.label());
            }
            Err(error) => {
                self.status_msg = format!("Could not switch to {}: {}", backend.label(), error);
                warn!("{}", self.status_msg);
            }
        }
    }

    fn save_app_settings(&mut self) {
        if let Err(error) = self.settings.save() {
            self.status_msg = format!("Settings save error: {error}");
            warn!("Could not save app settings: {error}");
        }
    }

    fn apply_osc_settings(&mut self) {
        // Release the old UDP listener before binding the replacement port.
        self.osc.take();
        match OscServer::new(
            self.settings.osc_listen_port,
            self.settings.osc_send_addr.clone(),
        ) {
            Ok(server) => {
                self.osc = Some(server);
                self.save_app_settings();
                self.status_msg =
                    format!("OSC listening on port {}", self.settings.osc_listen_port);
            }
            Err(error) => {
                self.status_msg = format!("OSC settings error: {error}");
                warn!("Could not apply OSC settings: {error}");
            }
        }
    }

    fn show_settings_window(&mut self, ctx: &Context) {
        if !self.settings_open {
            return;
        }

        let mut is_open = self.settings_open;

        // A foreground modal blocker consumes pointer input outside the Settings
        // window too, so clicks and wheel events cannot reach the Cue List.
        let screen_rect = ctx.input(|input| input.screen_rect());
        egui::Area::new(Id::new("settings_modal_input_blocker"))
            // Keep the blocker above the main workspace but below the Settings
            // window and its ComboBox/Popup layers.
            .order(egui::Order::Middle)
            .fixed_pos(screen_rect.min)
            .interactable(true)
            .show(ctx, |ui| {
                let (rect, _response) =
                    ui.allocate_exact_size(screen_rect.size(), egui::Sense::click_and_drag());
                ui.painter()
                    .rect_filled(rect, 0.0, egui::Color32::from_black_alpha(42));
            });

        egui::Window::new("CueLabs Settings")
            .open(&mut is_open)
            .default_size([820.0, 580.0])
            // No forced minimum or maximum: if content does not fit, scroll it.
            .resizable(true)
            .order(egui::Order::Foreground)
            .frame(egui::Frame::window(&ctx.style()).fill(Theme::BG_PANEL))
            .show(ctx, |ui| {
                egui::ScrollArea::horizontal()
                    .id_salt("settings_tab_strip")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for (tab, label) in [
                                (SettingsTab::Audio, "Audio"),
                                (SettingsTab::Osc, "OSC"),
                                (SettingsTab::Hotkeys, "Hotkeys"),
                                (SettingsTab::MidiControl, "MIDI Control"),
                                (SettingsTab::Midi, "MIDI Interfaces"),
                                (SettingsTab::Playback, "Playback"),
                                (SettingsTab::Backup, "Backups"),
                            ] {
                                ui.selectable_value(&mut self.settings_tab, tab, label);
                            }
                        });
                    });
                ui.separator();

                egui::ScrollArea::both()
                    .id_salt("settings_content_scroll")
                    // Let the window shrink to the current tab's content when
                    // possible; if the tab is taller than the window, scroll it.
                    .auto_shrink([false, true])
                    .max_height(ui.available_height().max(1.0))
                    .show(ui, |ui| {
                match self.settings_tab {
                    SettingsTab::Audio => {
                        ui.heading("Global audio interface");
                        let mut selected_backend = self.audio.backend;
                        ui.horizontal(|ui| {
                            ui.label("Audio driver");
                            egui::ComboBox::from_id_salt("settings_audio_backend")
                                .selected_text(selected_backend.label())
                                .show_ui(ui, |ui| {
                                    for backend in AudioBackend::ALL {
                                        if !cfg!(all(target_os = "windows", feature = "asio")) && *backend == AudioBackend::Asio {
                                            continue;
                                        }
                                        ui.selectable_value(&mut selected_backend, *backend, backend.label());
                                    }
                                });
                        });
                        if selected_backend != self.audio.backend {
                            self.switch_audio_backend(selected_backend);
                        }
                        if cfg!(all(target_os = "windows", not(feature = "asio"))) {
                            ui.colored_label(
                                Theme::TEXT_DIM,
                                "ASIO is not included in this build. Rebuild with --features asio to enable it.",
                            );
                        }
                        ui.label(format!("Backend: {}", self.audio.backend.label()));
                        let mut output = self.audio.current_device.clone();
                        ui.horizontal(|ui| {
                            ui.label("Output device");
                            egui::ComboBox::from_id_salt("settings_output_device")
                                .width(340.0)
                                .selected_text(&output)
                                .show_ui(ui, |ui| {
                                    for device in self.audio.output_devices.clone() {
                                        if ui.selectable_label(output == device, &device).clicked() {
                                            output = device.clone();
                                            self.audio.set_output_device(&device);
                                            for cue in &mut self.cue_list.cues {
                                                if matches!(&cue.cue_type, CueType::Audio) && cue.audio_channels > 0 {
                                                    let output_count = self.audio.output_channels_for_device(cue.io.output_device.as_deref());
                                                    let default_route_channels = self
                                                        .audio
                                                        .default_route_output_channels(cue.io.output_device.as_deref());
                                                    cue.io.ensure_channel_routes_with_defaults(
                                                        cue.audio_channels,
                                                        output_count,
                                                        &default_route_channels,
                                                    );
                                                }
                                            }
                                            self.settings.audio_output_device = Some(device);
                                            self.settings.output_channels = self.audio.output_channel_settings();
                                            self.save_app_settings();
                                        }
                                    }
                                });
                        });
                        ui.label(format!("Active stream outputs: {}", self.audio.output_channels));
                        let supported_layouts = self.audio.output_channel_layouts_for_device(Some(&output));
                        let layouts_text = if supported_layouts.is_empty() {
                            "not reported by driver".to_string()
                        } else {
                            supported_layouts.iter().map(|count| format!("{} ch", count)).collect::<Vec<_>>().join(", ")
                        };
                        ui.label(format!("Driver-supported channel layouts: {}", layouts_text));
                        if !supported_layouts.is_empty()
                            && self.audio.output_channels <= 2
                            && supported_layouts.iter().all(|count| *count <= 2)
                        {
                            ui.colored_label(
                                egui::Color32::YELLOW,
                                "Windows currently exposes this endpoint as stereo. Configure its speaker layout for 4.0/5.1 or enable the device's multichannel/ASIO driver mode, then reselect it here.",
                            );
                        }
                        let mut output_settings = self.audio.output_channel_settings();
                        let mut output_settings_changed = false;
                        let output_count = self
                            .audio
                            .output_channels_for_device(Some(&output))
                            .max(1);
                        let mut output_names =
                            self.audio.output_channel_names_for_device(Some(&output));
                        output_names.resize(output_count, String::new());
                        let mut output_names_changed = false;
                        let mut output_names_commit = false;
                        let channel_options: Vec<(usize, String)> = (0..output_count)
                            .map(|index| {
                                (
                                    index,
                                    self.audio.output_channel_label(Some(&output), index),
                                )
                            })
                            .collect();

                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new("Output channel names and levels").strong(),
                        );
                        ui.label(
                            RichText::new(
                                "Press Test to identify the physical connector, then enter its name. Calibration is saved separately for each device.",
                            )
                            .color(Theme::TEXT_DIM)
                            .size(11.0),
                        );
                        let output_order = self.audio.output_channel_display_order(Some(&output));
                        for index in output_order {
                            let Some(setting) = output_settings.get_mut(index) else {
                                continue;
                            };
                            ui.horizontal(|ui| {
                                output_settings_changed |=
                                    ui.checkbox(&mut setting.active, "Active").changed();
                                ui.label(format!("Driver ch. {}", index + 1));
                                let name_response = ui.add_sized(
                                    [190.0, 22.0],
                                    egui::TextEdit::singleline(&mut output_names[index])
                                        .hint_text(format!("Physical output {}", index + 1)),
                                );
                                output_names_changed |= name_response.changed();
                                output_names_commit |= name_response.lost_focus()
                                    || (name_response.has_focus()
                                        && ui.input(|input| input.key_pressed(Key::Enter)));
                                if ui.small_button("Test").clicked() {
                                    self.audio.test_output_channel(index);
                                }
                                output_settings_changed |= ui
                                    .add(
                                        egui::Slider::new(&mut setting.gain_db, -60.0..=18.0)
                                            .show_value(true)
                                            .custom_formatter(|value, _| {
                                                format!("{:+.1} dB", value)
                                            }),
                                    )
                                    .changed();
                            });
                        }
                        if output_settings_changed {
                            self.audio.set_output_channel_settings(output_settings.clone());
                            self.settings.output_channels = output_settings;
                            self.save_app_settings();
                        }
                        if output_names_changed {
                            self.audio
                                .set_output_channel_names_for_device(&output, output_names.clone());
                            let settings_key =
                                self.audio.output_device_settings_key(&output);
                            self.settings
                                .output_channel_names
                                .insert(settings_key, output_names);
                        }
                        if output_names_commit {
                            self.save_app_settings();
                        }

                        let mut default_pair =
                            self.audio.default_output_pair_for_device(Some(&output));
                        default_pair[0] = default_pair[0].min(output_count - 1);
                        default_pair[1] = default_pair[1].min(output_count - 1);
                        let mut default_pair_changed = false;
                        ui.horizontal(|ui| {
                            ui.label("Default stereo pair");
                            ui.label("L");
                            egui::ComboBox::from_id_salt(("default_output_left", output.clone()))
                                .width(150.0)
                                .selected_text(
                                    channel_options
                                        .get(default_pair[0])
                                        .map(|(_, label)| label.as_str())
                                        .unwrap_or("Output 1"),
                                )
                                .show_ui(ui, |ui| {
                                    for (index, label) in &channel_options {
                                        default_pair_changed |= ui
                                            .selectable_value(
                                                &mut default_pair[0],
                                                *index,
                                                label,
                                            )
                                            .changed();
                                    }
                                });
                            ui.label("R");
                            egui::ComboBox::from_id_salt(("default_output_right", output.clone()))
                                .width(150.0)
                                .selected_text(
                                    channel_options
                                        .get(default_pair[1])
                                        .map(|(_, label)| label.as_str())
                                        .unwrap_or("Output 1"),
                                )
                                .show_ui(ui, |ui| {
                                    for (index, label) in &channel_options {
                                        default_pair_changed |= ui
                                            .selectable_value(
                                                &mut default_pair[1],
                                                *index,
                                                label,
                                            )
                                            .changed();
                                    }
                                });
                        });
                        if default_pair_changed {
                            if output_count > 1 && default_pair[0] == default_pair[1] {
                                default_pair[1] = (default_pair[0] + 1) % output_count;
                            }
                            self.audio
                                .set_default_output_pair_for_device(&output, default_pair);
                            let settings_key =
                                self.audio.output_device_settings_key(&output);
                            self.settings
                                .default_output_pairs
                                .insert(settings_key, default_pair);
                            self.save_app_settings();
                        }

                        let mut input = self.audio.current_input_device.clone();
                        ui.horizontal(|ui| {
                            ui.label("Input device");
                            egui::ComboBox::from_id_salt("settings_input_device")
                                .width(340.0)
                                .selected_text(&input)
                                .show_ui(ui, |ui| {
                                    for device in self.audio.input_devices.clone() {
                                        if ui.selectable_label(input == device, &device).clicked() {
                                            input = device.clone();
                                            self.audio.set_input_device(&device);
                                            self.settings.audio_input_device = Some(device);
                                            self.save_app_settings();
                                        }
                                    }
                                });
                        });
                        ui.label(RichText::new(
                            "The selected output device and its driver channel count are global for the application.",
                        ).color(Theme::TEXT_DIM).size(11.0));
                    }
                    SettingsTab::Osc => {
                        ui.heading("Open Sound Control");
                        ui.horizontal(|ui| {
                            ui.label("Listen port");
                            ui.add(egui::DragValue::new(&mut self.settings.osc_listen_port).range(1..=65535));
                        });
                        ui.horizontal(|ui| {
                            ui.label("Send address");
                            ui.add(egui::TextEdit::singleline(&mut self.settings.osc_send_addr).desired_width(280.0));
                        });
                        if ui.button("Apply OSC settings").clicked() {
                            self.apply_osc_settings();
                        }
                        if let Some(server) = &self.osc {
                            ui.label(format!("Active: UDP {} → {}", server.listen_port, server.send_addr));
                        }
                    }
                    SettingsTab::Hotkeys => {
                        ui.heading("Application shortcuts");
                        if let Some(action) = &self.hotkey_capture {
                            ui.label(
                                RichText::new(format!("Press a key for {} (Esc cancels)", action.label()))
                                    .color(Theme::ACCENT),
                            );
                        }

                        let available_width = ui.available_width();
                        let column_count = if available_width >= 1180.0 {
                            4
                        } else if available_width >= 880.0 {
                            3
                        } else if available_width >= 560.0 {
                            2
                        } else {
                            1
                        };
                        let chunk_size = (AppAction::ALL.len() + column_count - 1) / column_count;
                        ui.columns(column_count, |columns| {
                            for (column_index, column_ui) in columns.iter_mut().enumerate() {
                                let start = column_index * chunk_size;
                                let end = (start + chunk_size).min(AppAction::ALL.len());
                                if start >= end {
                                    continue;
                                }
                                for action in &AppAction::ALL[start..end] {
                                    let action = (*action).clone();
                                    let binding = self.settings.hotkeys.bindings
                                        .get(&action)
                                        .cloned()
                                        .unwrap_or_else(|| "Unassigned".to_string());
                                    let is_capturing = self.hotkey_capture.as_ref() == Some(&action);
                                    column_ui.group(|ui| {
                                        ui.label(action.label());
                                        ui.horizontal(|ui| {
                                            ui.monospace(binding);
                                            if ui.button(if is_capturing { "Press key…" } else { "Change" }).clicked() {
                                                self.hotkey_capture = Some(action.clone());
                                            }
                                        });
                                    });
                                }
                            }
                        });
                        if ui.button("Restore default shortcuts").clicked() {
                            self.settings.hotkeys = crate::config::HotkeyConfig::default();
                            self.hotkey_capture = None;
                            self.save_app_settings();
                        }
                    }
                    SettingsTab::MidiControl => {
                        ui.heading("MIDI Control bindings");
                        ui.label(
                            RichText::new("Choose Learn beside a command, then press a button on your MIDI controller. Note On and CC button press/release messages are supported.")
                                .color(Theme::TEXT_DIM)
                                .size(11.0),
                        );
                        if let Some(action) = self.midi_learn_action.clone() {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!("Listening for MIDI input for: {}", action.label()))
                                        .color(Theme::ACCENT)
                                        .strong(),
                                );
                                if ui.small_button("Cancel learn").clicked() {
                                    self.midi_learn_action = None;
                                }
                            });
                        }
                        ui.label(format!(
                            "Input device: {}",
                            self.midi.current_port.as_deref().unwrap_or("No MIDI input selected")
                        ));

                        let available_width = ui.available_width();
                        let column_count = if available_width >= 1180.0 {
                            4
                        } else if available_width >= 880.0 {
                            3
                        } else if available_width >= 560.0 {
                            2
                        } else {
                            1
                        };
                        let chunk_size = (AppAction::ALL.len() + column_count - 1) / column_count;
                        ui.columns(column_count, |columns| {
                            for (column_index, column_ui) in columns.iter_mut().enumerate() {
                                let start = column_index * chunk_size;
                                let end = (start + chunk_size).min(AppAction::ALL.len());
                                if start >= end {
                                    continue;
                                }
                                for action in &AppAction::ALL[start..end] {
                                    let action = (*action).clone();
                                    let labels: Vec<String> = self.settings.midi_controls
                                        .iter()
                                        .filter(|binding| binding.action == action)
                                        .map(MidiControlBinding::label)
                                        .collect();
                                    let binding_text = if labels.is_empty() {
                                        "Not assigned".to_string()
                                    } else {
                                        labels.join(", ")
                                    };
                                    let is_learning = self.midi_learn_action.as_ref() == Some(&action);
                                    column_ui.group(|ui| {
                                        ui.label(action.label());
                                        ui.label(
                                            RichText::new(binding_text)
                                                .color(Theme::TEXT_DIM)
                                                .size(10.5),
                                        );
                                        ui.horizontal(|ui| {
                                            if ui.button(if is_learning { "Listening…" } else { "Learn" }).clicked() {
                                                self.midi_learn_action = Some(action.clone());
                                                self.status_msg = format!("Press a MIDI control for {}", action.label());
                                            }
                                            if !labels.is_empty() && ui.small_button("Clear").clicked() {
                                                self.settings.midi_controls.retain(|binding| binding.action != action);
                                                if self.midi_learn_action.as_ref() == Some(&action) {
                                                    self.midi_learn_action = None;
                                                }
                                                self.save_app_settings();
                                            }
                                        });
                                    });
                                }
                            }
                        });
                    }
                    SettingsTab::Midi => {
                        ui.heading("MIDI interfaces");
                        ui.horizontal(|ui| {
                            ui.label("MIDI input");
                            let input_label = self.midi.current_port.clone()
                                .unwrap_or_else(|| "No input selected".to_string());
                            egui::ComboBox::from_id_salt("settings_midi_input")
                                .width(320.0)
                                .selected_text(input_label)
                                .show_ui(ui, |ui| {
                                    for port in self.midi.port_names.clone() {
                                        if ui.selectable_label(self.midi.current_port.as_deref() == Some(port.as_str()), &port).clicked() {
                                            self.midi.refresh_and_connect(Some(&port));
                                            self.midi_cc_pressed.clear();
                                            self.settings.midi_input_port = Some(port);
                                            self.save_app_settings();
                                        }
                                    }
                                });
                        });
                        ui.horizontal(|ui| {
                            ui.label("MIDI output");
                            let output_label = self.midi_out.current_port.clone()
                                .unwrap_or_else(|| "No output selected".to_string());
                            egui::ComboBox::from_id_salt("settings_midi_output")
                                .width(320.0)
                                .selected_text(output_label)
                                .show_ui(ui, |ui| {
                                    for port in self.midi_out.port_names.clone() {
                                        if ui.selectable_label(self.midi_out.current_port.as_deref() == Some(port.as_str()), &port).clicked() {
                                            self.midi_out.refresh_and_connect(Some(&port));
                                            self.settings.midi_output_port = Some(port);
                                            self.save_app_settings();
                                        }
                                    }
                                });
                        });
                        if ui.button("Refresh MIDI ports").clicked() {
                            self.midi.refresh_and_connect(self.settings.midi_input_port.as_deref());
                            self.midi_cc_pressed.clear();
                            self.midi_out.refresh_and_connect(self.settings.midi_output_port.as_deref());
                        }
                    }
                    SettingsTab::Playback => {
                        ui.heading("Stop All Cues");
                        ui.horizontal(|ui| {
                            ui.label("Fade duration");
                            ui.add(egui::Slider::new(&mut self.settings.stop_all_fade_secs, 0.0..=10.0)
                                .step_by(0.1));
                            ui.label("seconds");
                        });
                        ui.label(RichText::new("0.0 s stops immediately. A longer value fades active audio cues before stopping them.")
                            .color(Theme::TEXT_DIM).size(11.0));
                        if ui.button("Save playback settings").clicked() {
                            self.save_app_settings();
                        }
                    }
                    SettingsTab::Backup => {
                        ui.heading("Project backups");
                        ui.label(
                            RichText::new(
                                "CueLabs creates a lightweight event snapshot after every successful project save and periodically when the project has changed.",
                            )
                            .color(Theme::TEXT_DIM)
                            .size(11.0),
                        );
                        ui.add_space(8.0);

                        let interval_changed = ui
                            .horizontal(|ui| {
                                ui.label("Automatic backup interval");
                                ui.add(
                                    egui::DragValue::new(
                                        &mut self.settings.backup_interval_minutes,
                                    )
                                    .range(0..=240)
                                    .speed(1)
                                    .suffix(" min"),
                                )
                                .changed()
                            })
                            .inner;

                        ui.label(
                            RichText::new("0 disables timed backups. The periodic timer is independent of manual saves; unchanged projects are not backed up repeatedly.")
                                .color(Theme::TEXT_DIM)
                                .size(11.0),
                        );
                        ui.label(
                            RichText::new("The backup folder stores event/project data only, not audio media. CueLabs keeps the 10 newest snapshots and removes the oldest automatically.")
                                .color(Theme::TEXT_DIM)
                                .size(11.0),
                        );
                        ui.add_space(6.0);
                        if ui
                            .add_enabled(self.project_path.is_some(), egui::Button::new("Create backup now"))
                            .clicked()
                        {
                            self.create_project_backup("manual");
                        }

                        if interval_changed {
                            self.last_periodic_backup_at = Instant::now();
                            self.save_app_settings();
                        }
                    }
                }
                    });
            });
        self.settings_open = is_open;
        if !is_open {
            self.hotkey_capture = None;
            self.midi_learn_action = None;
        }
    }

    fn show_drop_overlay(&self, ctx: &Context) {
        let hovering_files = ctx.input(|input| !input.raw.hovered_files.is_empty());
        if !hovering_files {
            return;
        }
        ctx.request_repaint();
        egui::Area::new(Id::new("external_audio_drop_hint"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .fill(Theme::BG_PANEL)
                    .stroke(egui::Stroke::new(2.0_f32, Theme::ACCENT))
                    .inner_margin(egui::Margin::symmetric(28_i8, 18_i8))
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new("Drop audio files to add them to the project")
                                    .color(Theme::TEXT)
                                    .size(18.0)
                                    .strong(),
                            );
                            ui.label(
                                RichText::new("WAV · MP3 · FLAC · OGG · AIFF · M4A")
                                    .color(Theme::TEXT_DIM)
                                    .size(12.0),
                            );
                        });
                    });
            });
    }

    fn handle_drop(&mut self, ctx: &Context) {
        if self.settings_open {
            return;
        }
        // dropped_files живёт ровно один кадр — клонируем СРАЗУ, до любых действий
        let dropped: Vec<PathBuf> = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| {
                    file.path.clone().or_else(|| {
                        let candidate = PathBuf::from(&file.name);
                        if candidate.is_file() {
                            Some(candidate)
                        } else {
                            None
                        }
                    })
                })
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
                if ui.button("Save As…    Ctrl+Shift+S").clicked() {
                    self.save_as_project();
                    ui.close_menu();
                }
                if ui.button("Save as Bundle…    Ctrl+Alt+S").clicked() {
                    self.save_as_bundle();
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Add audio files… Ctrl+I").clicked() {
                    self.add_files();
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Settings…").clicked() {
                    self.settings_open = true;
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
                        target_volume: None,
                        fade_curve: FadeCurve::Linear,
                        custom_fade_points: vec![[0.0, 0.0], [1.0, 1.0]],
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
                if ui.button("📡 Add OSC Cue").clicked() {
                    self.add_typed_cue(CueType::Osc {
                        address: "/cuelabs/event".to_string(),
                        args: Vec::new(),
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
            });
        });
    }

    // ── Нижняя панель (Inspector / Waveform / Playback / I/O Routing / MIDI) ──

    fn bottom_panel(&mut self, ui: &mut egui::Ui) {
        // Never impose a minimum height from the content: the resizable
        // TopBottomPanel owns the viewport, and the Cue List shrinks with it.
        let full_panel_rect = ui.max_rect();
        let _background_interaction = ui.interact(
            full_panel_rect,
            Id::new("bottom_panel_full_area_blocker"),
            egui::Sense::click_and_drag(),
        );

        let selected_type = self
            .selected
            .and_then(|id| self.cue_list.find(id))
            .map(|cue| cue.cue_type.clone());
        let selected_is_audio = matches!(selected_type.as_ref(), Some(CueType::Audio));
        let selected_is_midi = matches!(selected_type.as_ref(), Some(CueType::Midi { .. }));

        if !selected_is_audio
            && matches!(
                self.active_panel,
                Panel::Waveform | Panel::Playback | Panel::IoRouting
            )
        {
            self.active_panel = Panel::Inspector;
        }
        if !selected_is_midi && self.active_panel == Panel::Midi {
            self.active_panel = Panel::Inspector;
        }

        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.active_panel, Panel::Inspector, "⚙ Inspector");
            if selected_is_audio {
                ui.selectable_value(
                    &mut self.active_panel,
                    Panel::Waveform,
                    "🌊 Waveform & Region/Loop",
                );
                ui.selectable_value(&mut self.active_panel, Panel::Playback, "🎚 Playback");
                ui.selectable_value(&mut self.active_panel, Panel::IoRouting, "🔌 I/O Routing");
            }
            if selected_is_midi {
                ui.selectable_value(&mut self.active_panel, Panel::Midi, "🎹 MIDI Settings");
            }
            ui.selectable_value(&mut self.active_panel, Panel::Report, "📋 Report");
        });
        ui.separator();

        // Use the panel's current available height, not the saved previous
        // height. This is essential: using the old value here prevents the
        // TopBottomPanel from shrinking after a user drag.
        let content_height = ui.available_height().max(1.0);
        egui::ScrollArea::vertical()
            .id_salt("bottom_panel_content_scroll")
            .auto_shrink([false, false])
            .max_height(content_height)
            .show(ui, |ui| {
                match self.active_panel {
                    Panel::Inspector => {
                        let cue_targets: Vec<(Uuid, String, Option<PathBuf>)> = self
                            .cue_list
                            .cues
                            .iter()
                            .filter(|cue| {
                                matches!(&cue.cue_type, CueType::Audio | CueType::Group { .. })
                            })
                            .map(|cue| {
                                let label = if cue.number.trim().is_empty() {
                                    format!("{} ({})", cue.name, cue.cue_type.label())
                                } else {
                                    format!(
                                        "#{} — {} ({})",
                                        cue.number,
                                        cue.name,
                                        cue.cue_type.label()
                                    )
                                };
                                let file_path = matches!(&cue.cue_type, CueType::Audio)
                                    .then(|| cue.file_path.clone());
                                (cue.id, label, file_path)
                            })
                            .collect();
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
                            let mut inspector_action = None;
                            if let Some(cue) = self.cue_list.find_mut(id) {
                                inspector_action =
                                    Inspector::show_inspector(ui, cue, &cue_targets, &all_cues);
                            }
                            if let Some(InspectorAction::SetAudioFile(path)) = inspector_action {
                                self.set_audio_cue_file(id, path);
                            }
                        } else {
                            ui.label(
                                egui::RichText::new("Select a cue to inspect its properties")
                                    .color(Theme::TEXT_DIM),
                            );
                        }
                    }

                    Panel::Waveform => {
                        if let Some(id) = self.selected {
                            let was_active = self
                                .cue_list
                                .find(id)
                                .map(|cue| cue.is_active())
                                .unwrap_or(false);
                            let seek_position = if let Some(cue) = self.cue_list.find_mut(id) {
                                WaveformEditor::show(ui, cue)
                            } else {
                                None
                            };
                            if let Some(position_secs) = seek_position {
                                // A click on a stopped cue is a stored start marker, not
                                // a paused audio player. Resume Selected must then start
                                // the cue from this position instead of resuming a player
                                // that has not been created yet.
                                if !was_active {
                                    if let Some(cue) = self.cue_list.find_mut(id) {
                                        cue.state = CueState::Stopped;
                                    }
                                }
                                self.audio.send(AudioCommand::Seek { id, position_secs });
                                self.status_msg =
                                    format!("⏸ Position set to {:.2}s", position_secs);
                            }
                        } else {
                            ui.label(
                                egui::RichText::new("Select an audio cue to edit waveform & loop")
                                    .color(Theme::TEXT_DIM),
                            );
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
                            ui.label(
                                egui::RichText::new("Select a cue to configure Playback & Fades")
                                    .color(Theme::TEXT_DIM),
                            );
                        }
                    }

                    Panel::IoRouting => {
                        let selected_id = self.selected;
                        let routes_changed = if let Some(id) = selected_id {
                            if let Some(cue) = self.cue_list.find_mut(id) {
                                Inspector::show_io_routing(ui, Some(cue), &mut self.audio)
                            } else {
                                Inspector::show_io_routing(ui, None, &mut self.audio)
                            }
                        } else {
                            Inspector::show_io_routing(ui, None, &mut self.audio)
                        };

                        if routes_changed {
                            if let Some(id) = selected_id {
                                if let Some(cue) = self.cue_list.find(id) {
                                    self.audio.send(AudioCommand::SetRouting {
                                        id,
                                        routes: cue.io.channel_routes.clone(),
                                    });
                                }
                            }
                            self.settings.output_channels = self.audio.output_channel_settings();
                            self.save_app_settings();
                        }
                    }

                    Panel::Midi => {
                        if let Some(id) = self.selected {
                            if let Some(cue) = self.cue_list.find_mut(id) {
                                Inspector::show_midi(ui, cue, &mut self.midi_out);
                            }
                        }
                    }

                    Panel::Report => {
                        let text = self.report.to_text_summary();
                        let report_width = ui.available_width();
                        ui.set_width(report_width);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(text)
                                    .monospace()
                                    .size(11.0)
                                    .color(Theme::TEXT_DIM),
                            )
                            .wrap(),
                        );
                    }
                }
            });
    }
}

impl eframe::App for CueLabsApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        if !self.startup_frame_logged {
            info!("CueLabs GUI reached its first frame");
            self.startup_frame_logged = true;
        }

        // A previous build could have persisted the runaway panel's expanded
        // height. Reset only clearly screen-sized values once, then preserve
        // normal user resizing from this version onward.
        let screen_height = ctx.input(|input| input.screen_rect().height());
        if screen_height > 0.0 && self.bottom_panel_height > screen_height * 0.72 {
            self.bottom_panel_height = 225.0_f32.min((screen_height * 0.4).max(80.0));
            self.settings.bottom_panel_height = self.bottom_panel_height;
            self.save_app_settings();
        }

        self.poll_pending_triggers();
        self.poll_audio();
        self.poll_midi();
        self.poll_osc();
        self.poll_audio_file_metadata();
        self.poll_periodic_backup();
        self.handle_keyboard(ctx);
        self.handle_drop(ctx);

        if self.cue_list.active_count() > 0 || !self.pending_triggers.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(30));
        } else if (self.settings.backup_interval_minutes > 0
            && self.project_path.is_some()
            && !self.project_needs_save_as)
            || self
                .cue_list
                .cues
                .iter()
                .any(|cue| matches!(&cue.cue_type, CueType::Audio) && !cue.file_path.is_file())
        {
            // Timed backups and missing-media recovery are checked once per
            // second even while the show is otherwise idle.
            ctx.request_repaint_after(Duration::from_secs(1));
        }

        // ── Верхнее меню ──────────────────────────────────────────────────
        egui::TopBottomPanel::top("menu")
            .frame(
                egui::Frame::NONE
                    .fill(Theme::BG_PANEL)
                    .inner_margin(egui::Margin::symmetric(4_i8, 2_i8)),
            )
            .show(ctx, |ui| {
                ui.add_enabled_ui(!self.settings_open, |ui| {
                    self.menu_bar(ui);
                });
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
                                "🔊 {} · {} ({} ch) | 🎙 In: {}",
                                self.audio.backend.label(),
                                self.audio.current_device,
                                self.audio.output_channels,
                                self.audio.current_input_device
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
                ui.add_enabled_ui(!self.settings_open, |ui| {
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
            });

        // ── Нижняя панель инспектора / вейвформы / роутинга / MIDI ────────
        if self.bottom_panel_visible {
            let lower_panel = egui::TopBottomPanel::bottom("bottom_inspector")
                .resizable(true)
                .height_range(80.0..=1200.0)
                .default_height(self.bottom_panel_height)
                .frame(
                    egui::Frame::NONE
                        .fill(Theme::BG_PANEL)
                        .inner_margin(egui::Margin::same(8_i8)),
                )
                .show(ctx, |ui| {
                    ui.add_enabled_ui(!self.settings_open, |ui| {
                        // Reserve the complete lower-panel rectangle for this panel.
                        // Empty space must not pass pointer clicks through to Cue List.
                        let panel_content_rect = ui.max_rect();
                        ui.interact(
                            panel_content_rect,
                            Id::new("bottom_panel_full_area_input_blocker"),
                            egui::Sense::click_and_drag(),
                        );
                        self.bottom_panel(ui);
                    });
                });
            let panel_rect = lower_panel.response.rect;
            let measured_panel_height = panel_rect.height();
            egui::Area::new(Id::new("hide_bottom_panel_button"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -42.0))
                .show(ctx, |ui| {
                    if ui.small_button("Hide panel ▾").clicked() {
                        self.bottom_panel_visible = false;
                    }
                });
            let resize_handle_rect = egui::Rect::from_min_max(
                egui::pos2(panel_rect.left(), panel_rect.top() - 7.0),
                egui::pos2(panel_rect.right(), panel_rect.top() + 7.0),
            );
            let dragging_panel_handle = ctx.input(|input| {
                (input.pointer.button_down(egui::PointerButton::Primary)
                    || input.pointer.button_released(egui::PointerButton::Primary))
                    && input
                        .pointer
                        .interact_pos()
                        .map(|position| resize_handle_rect.contains(position))
                        .unwrap_or(false)
            });
            if dragging_panel_handle
                && measured_panel_height.is_finite()
                && (measured_panel_height - self.bottom_panel_height).abs() > 1.0
            {
                self.bottom_panel_height = measured_panel_height.clamp(80.0, 1200.0);
                self.settings.bottom_panel_height = self.bottom_panel_height;
                self.bottom_panel_dirty = true;
            }
            if self.bottom_panel_dirty && !ctx.input(|input| input.pointer.any_down()) {
                if measured_panel_height.is_finite() {
                    self.bottom_panel_height = measured_panel_height.clamp(80.0, 1200.0);
                    self.settings.bottom_panel_height = self.bottom_panel_height;
                }
                self.save_app_settings();
                self.bottom_panel_dirty = false;
            }
        } else if !self.settings_open {
            egui::Area::new(Id::new("show_bottom_panel_button"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -42.0))
                .show(ctx, |ui| {
                    if ui.button("Show panel ▴").clicked() {
                        self.bottom_panel_visible = true;
                    }
                });
        }

        // ── Боковая панель Notes выбранного события ────────────────────────
        if self.notes_sidebar_open && !self.settings_open {
            egui::SidePanel::right("cue_notes_sidebar")
                .resizable(true)
                .default_width(235.0)
                .width_range(150.0..=520.0)
                .frame(
                    egui::Frame::NONE
                        .fill(Theme::BG_PANEL)
                        .inner_margin(egui::Margin::same(8_i8)),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("Notes");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("×").clicked() {
                                self.notes_sidebar_open = false;
                            }
                        });
                    });
                    ui.separator();
                    if let Some(selected_id) = self.selected {
                        if let Some(cue) = self.cue_list.find_mut(selected_id) {
                            ui.label(egui::RichText::new(&cue.name).strong().color(Theme::TEXT));
                            ui.add_space(4.0);
                            let notes_width = ui.available_width();
                            ui.add(
                                egui::TextEdit::multiline(&mut cue.notes)
                                    .font(egui::FontId::proportional(16.0))
                                    .desired_width(notes_width)
                                    .desired_rows(12)
                                    .hint_text("Notes for selected event…"),
                            );
                        } else {
                            ui.label("Select an event to edit its notes.");
                        }
                    } else {
                        ui.label("Select an event to edit its notes.");
                    }
                });
        }

        // ── Центральный список событий (Cue List) ─────────────────────────
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(Theme::BG_DARK)
                    .inner_margin(egui::Margin::same(0_i8)),
            )
            .show(ctx, |ui| {
                if self.settings_open {
                    let blocked_rect = ui.max_rect();
                    ui.painter().rect_filled(blocked_rect, 0.0, Theme::BG_DARK);
                    ui.allocate_rect(blocked_rect, egui::Sense::click_and_drag());
                } else {
                    ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        let title = self.project.settings.name.clone();
                        let mut rename_commit: Option<String> = None;
                        let mut rename_cancel = false;

                        if let Some(edit_name) = self.project_name_edit.as_mut() {
                            let response = ui.add(
                                egui::TextEdit::singleline(edit_name)
                                    .desired_width(190.0)
                                    .font(egui::FontId::proportional(14.0)),
                            );
                            if self.project_name_edit_focus_pending {
                                response.request_focus();
                                self.project_name_edit_focus_pending = false;
                            }

                            let enter_pressed = ui.input(|input| input.key_pressed(Key::Enter));
                            let escape_pressed = ui.input(|input| input.key_pressed(Key::Escape));
                            if response.has_focus() && escape_pressed {
                                rename_cancel = true;
                            } else if (response.has_focus() && enter_pressed)
                                || response.lost_focus()
                            {
                                rename_commit = Some(edit_name.clone());
                            }
                        } else {
                            let title_response = ui.add(
                                egui::Label::new(
                                    egui::RichText::new(title.clone())
                                        .color(Theme::TEXT)
                                        .size(14.0),
                                )
                                .sense(egui::Sense::click()),
                            );
                            let title_response =
                                title_response.on_hover_text("Double-click to rename project");
                            if title_response.double_clicked() {
                                self.project_name_edit = Some(title);
                                self.project_name_edit_focus_pending = true;
                            }
                        }

                        if rename_cancel {
                            self.project_name_edit = None;
                            self.project_name_edit_focus_pending = false;
                        } else if let Some(new_name) = rename_commit {
                            self.project_name_edit = None;
                            self.project_name_edit_focus_pending = false;
                            self.rename_project(new_name);
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
                                target_volume: None,
                                fade_curve: FadeCurve::Linear,
                                custom_fade_points: vec![[0.0, 0.0], [1.0, 1.0]],
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
                        if ui.button("📡 OSC").clicked() {
                            self.add_typed_cue(CueType::Osc {
                                address: "/cuelabs/event".to_string(),
                                args: Vec::new(),
                            });
                        }
                        if ui.button("📁 Group").clicked() {
                            // Create an empty group directly after the selected cue.
                            // The selected cue stays outside until it is explicitly moved in.
                            self.add_typed_cue(CueType::Group {
                                mode: GroupMode::Normal,
                                children: Vec::new(),
                                fade_in_secs: 0.0,
                                fade_out_secs: 0.0,
                            });
                        }

                        if ui
                            .selectable_label(self.notes_sidebar_open, "📝 Notes")
                            .clicked()
                        {
                            self.notes_sidebar_open = !self.notes_sidebar_open;
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("⚙ Settings").clicked() {
                                self.settings_open = true;
                            }
                        });
                    });
                    ui.add(egui::Separator::default().horizontal());

                    let action = CueTable::show(
                        ui,
                        &mut self.cue_list.cues,
                        &mut self.selected,
                        &mut self.selected_ids,
                        &mut self.selection_anchor,
                        &mut self.editing,
                        &mut self.editing_focus_pending,
                        &mut self.cue_column_widths,
                        &mut self.expanded_groups,
                    );
                    if ctx.input(|input| input.pointer.any_released())
                        && self.settings.cue_column_widths != self.cue_column_widths
                    {
                        self.settings.cue_column_widths = self.cue_column_widths;
                        self.save_app_settings();
                    }

                    if let Some(action) = action {
                        match action {
                            CueTableAction::Deselect => {
                                self.selected = None;
                                self.selected_ids.clear();
                                self.selection_anchor = None;
                                self.editing = None;
                                self.editing_focus_pending = false;
                            }
                            CueTableAction::Play(id) => {
                                self.select_only(id);
                                self.trigger_cue(id);
                            }
                            CueTableAction::Stop(id) => {
                                self.select_only(id);
                                self.stop_selected();
                            }
                            CueTableAction::Pause(id) => {
                                self.select_only(id);
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
                                self.editing_focus_pending = true;
                            }
                            CueTableAction::EditConfirm(id, field, new_val) => {
                                match field {
                                    EditField::Number => {
                                        if let Some(cue) = self.cue_list.find_mut(id) {
                                            cue.number = new_val.trim().to_string();
                                        }
                                    }
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
                                self.editing_focus_pending = false;
                            }
                            CueTableAction::ClearNumber(id) => {
                                if let Some(cue) = self.cue_list.find_mut(id) {
                                    cue.number.clear();
                                }
                            }
                            CueTableAction::EditCancel => {
                                self.editing = None;
                                self.editing_focus_pending = false;
                            }
                            CueTableAction::SetColor(id, color) => {
                                if let Some(cue) = self.cue_list.find_mut(id) {
                                    cue.color = color;
                                }
                            }
                            CueTableAction::MoveMultipleToPosition(
                                dragged_ids,
                                parent_id,
                                sibling_index,
                            ) => {
                                self.handle_drag_drop_position(
                                    dragged_ids,
                                    parent_id,
                                    sibling_index,
                                );
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
                                            let target_label =
                                                if target_cue.number.trim().is_empty() {
                                                    target_cue.name.clone()
                                                } else {
                                                    format!(
                                                        "#{} {}",
                                                        target_cue.number, target_cue.name
                                                    )
                                                };
                                            status_msg = Some(format!(
                                                "Target set for {} → {}",
                                                target_label, dragged_name
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
                }
            });
        self.show_settings_window(ctx);
        self.show_drop_overlay(ctx);
    }
}

#[cfg(test)]
mod normal_group_tests {
    use super::normal_group_manual_stop_in;
    use crate::cue::{ContinueMode, Cue, CueType, GroupMode};
    use std::collections::HashSet;

    fn audio_cue(mode: ContinueMode) -> Cue {
        let mut cue = Cue::new_typed(CueType::Audio);
        cue.continue_mode = mode;
        cue
    }

    fn normal_group(children: &[Cue]) -> Cue {
        Cue::new_typed(CueType::Group {
            mode: GroupMode::Normal,
            children: children.iter().map(|cue| cue.id).collect(),
            fade_in_secs: 0.0,
            fade_out_secs: 0.0,
        })
    }

    #[test]
    fn final_nothing_means_chain_reaches_group_end_without_expansion() {
        let children = vec![
            audio_cue(ContinueMode::AutoContinue),
            audio_cue(ContinueMode::AutoContinue),
            audio_cue(ContinueMode::AutoFollow),
            audio_cue(ContinueMode::AutoFollow),
            audio_cue(ContinueMode::AutoContinue),
            audio_cue(ContinueMode::Nothing),
        ];
        let group = normal_group(&children);
        let mut cues = children;
        cues.push(group.clone());

        assert_eq!(
            normal_group_manual_stop_in(&cues, group.id, &mut HashSet::new()),
            None
        );
    }

    #[test]
    fn nothing_before_later_children_selects_the_manual_boundary() {
        let children = vec![
            audio_cue(ContinueMode::AutoContinue),
            audio_cue(ContinueMode::AutoFollow),
            audio_cue(ContinueMode::AutoContinue),
            audio_cue(ContinueMode::Nothing),
            audio_cue(ContinueMode::AutoContinue),
            audio_cue(ContinueMode::Nothing),
        ];
        let group = normal_group(&children);
        let boundary_id = children[3].id;
        let mut cues = children;
        cues.push(group.clone());

        assert_eq!(
            normal_group_manual_stop_in(&cues, group.id, &mut HashSet::new()),
            Some((vec![group.id], boundary_id))
        );
    }

    #[test]
    fn nested_manual_boundary_returns_full_group_path() {
        let nested_children = vec![
            audio_cue(ContinueMode::AutoFollow),
            audio_cue(ContinueMode::Nothing),
            audio_cue(ContinueMode::AutoContinue),
        ];
        let nested = Cue::new_typed(CueType::Group {
            mode: GroupMode::Normal,
            children: nested_children.iter().map(|cue| cue.id).collect(),
            fade_in_secs: 0.0,
            fade_out_secs: 0.0,
        });
        let after_nested = audio_cue(ContinueMode::Nothing);
        let outer = Cue::new_typed(CueType::Group {
            mode: GroupMode::Normal,
            children: vec![nested.id, after_nested.id],
            fade_in_secs: 0.0,
            fade_out_secs: 0.0,
        });
        let boundary_id = nested_children[1].id;
        let mut cues = nested_children;
        cues.push(nested.clone());
        cues.push(after_nested);
        cues.push(outer.clone());

        assert_eq!(
            normal_group_manual_stop_in(&cues, outer.id, &mut HashSet::new()),
            Some((vec![outer.id, nested.id], boundary_id))
        );
    }
}
