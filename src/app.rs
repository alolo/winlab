// src/app.rs — главное состояние приложения Theatre Show Cue

use crate::audio::device_registry::DeviceRegistryMonitor;
use crate::audio::engine::{AudioCommand, AudioEngine};
use crate::audio::waveform::{generate_waveform, get_channel_count, get_duration};
use crate::config::{
    AppAction, AppSettings, AudioBackend, AudioPerformanceSettings, MidiControlBinding,
    MidiControlKind, ThemePreset, UiDesign,
};
use crate::cue::{
    ContinueMode, Cue, CueList, CueState, CueType, FadeCurve, GroupMode, MidiMessageKind,
    OscCueArg, WaitPhase,
};
use crate::export::session_report::SessionReport;
use crate::midi::input::{MidiInputManager, MidiOutputManager};
use crate::osc::server::OscServer;
use crate::project::{asset_manager::AssetManager, save_load::Project};
use crate::ui::{
    cue_table::{visible_cue_ids, visible_navigation_cue_ids, CueTable, CueTableAction, EditField},
    inspector::{Inspector, InspectorAction},
    theme::Theme,
    transport::{TransportAction, TransportBar},
    waveform_editor::WaveformEditor,
};

use eframe::egui::{self, Context, Id, Key, RichText};
use rfd::FileDialog;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use uuid::Uuid;

const INTERNAL_CUE_CLIPBOARD_MARKER: &str = "TheatreShowCue::InternalCueClipboard::v1";
const WAVEFORM_BUCKETS: usize = 4096;

fn edit_rgb_color(ui: &mut egui::Ui, label: &str, rgb: &mut [u8; 3]) -> bool {
    ui.horizontal(|ui| {
        ui.label(label);
        let mut color = egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
        let changed = ui.color_edit_button_srgba(&mut color).changed();
        if changed {
            *rgb = [color.r(), color.g(), color.b()];
        }
        changed
    })
    .inner
}

fn ensure_project_extension(path: PathBuf) -> PathBuf {
    // New projects use .tsq. Keep .clabs intact when saving an older project
    // in place so existing files remain editable without forced conversion.
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    if extension.eq_ignore_ascii_case("tsq") || extension.eq_ignore_ascii_case("clabs") {
        path
    } else {
        path.with_extension("tsq")
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

fn cue_uses_output_endpoint(
    cue: &Cue,
    backend: AudioBackend,
    device_name: &str,
    endpoint_id: &str,
) -> bool {
    let cue_backend = cue.io.output_backend.unwrap_or(backend);
    if cue_backend != backend {
        return false;
    }

    match cue.io.output_device.as_deref() {
        None => cue
            .io
            .output_device_id
            .as_deref()
            .map_or(true, |saved_id| saved_id == endpoint_id),
        Some(saved_name) => match cue.io.output_device_id.as_deref() {
            Some(saved_id) => saved_id == endpoint_id || saved_name == device_name,
            None => saved_name == device_name,
        },
    }
}

fn normalize_buffer_selection(
    backend: AudioBackend,
    selected: Option<u32>,
    supported_sizes: &[u32],
) -> Option<u32> {
    match selected {
        Some(frames) if backend != AudioBackend::Wasapi && !supported_sizes.contains(&frames) => {
            None
        }
        other => other,
    }
}

/// Upgrade saved stereo-pair selections from native ASIO indices to stable
/// logical TaSQ outputs. This is generic for all backends, not TASCAM-specific.
fn migrate_saved_stereo_pairs_to_logical(settings: &mut AppSettings) -> bool {
    if settings.audio_routing_schema >= 3 {
        return false;
    }
    for (device_key, pair) in settings.default_output_pairs.iter_mut() {
        if let Some(order) = settings.output_channel_orders.get(device_key) {
            for index in pair.iter_mut() {
                if let Some(logical_index) = order.iter().position(|native| *native == *index) {
                    *index = logical_index;
                }
            }
        }
    }
    settings.audio_routing_schema = 3;
    true
}

fn cue_owns_audio_stream(cue: &Cue) -> bool {
    matches!(&cue.cue_type, CueType::Audio)
        || (matches!(&cue.cue_type, CueType::Play { target_id: None })
            && !cue.file_path.as_os_str().is_empty())
}

#[derive(Debug, Clone, PartialEq)]
enum Panel {
    Inspector,
    Waveform,
    Playback,
    IoRouting,
    Trim,
    Midi,
    Report,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    Audio,
    Theme,
    Osc,
    Hotkeys,
    Midi,
    MidiControl,
    Playback,
    BackupLogs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingAction {
    ExecuteCue,
    TriggerCue,
    BeginPostWait,
    ClearWait,
    CompleteCue,
}

#[derive(Debug, Clone)]
struct PendingTrigger {
    cue_id: Uuid,
    /// Cue that scheduled this trigger, used to cancel its automatic chain on Stop/Pause.
    source_cue_id: Option<Uuid>,
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
        .filter(|cue| !matches!(&cue.cue_type, CueType::Comment { .. }))
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

pub struct TheatreShowCueApp {
    audio: AudioEngine,
    device_registry_monitor: DeviceRegistryMonitor,
    /// True after the active endpoint disappears; the next explicit selection
    /// of that same endpoint rebuilds the stream after it is reconnected.
    active_output_needs_reopen: bool,
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
    /// One-shot Cue List scroll target used by Select Next/Previous navigation.
    scroll_to_cue: Option<Uuid>,
    selected_ids: HashSet<Uuid>,
    selection_anchor: Option<Uuid>,
    cue_clipboard: Vec<Cue>,
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
    bottom_panel_visible: bool,
    /// True while the lower inspector's resize handle owns the primary pointer drag.
    /// CueTable must not interpret that same drag as a cue-row move.
    bottom_panel_resize_active: bool,
    /// Also suppress the pointer-release frame so the splitter's mouse-up cannot
    /// be interpreted as a click on the last visible cue row.
    bottom_panel_resize_release_guard: bool,
    notes_sidebar_open: bool,
    fade_started_at: HashMap<Uuid, Instant>,
    fade_managed_targets: HashSet<Uuid>,
    /// Current audio playback generation per cue. Late updates from a stopped
    /// playback instance are ignored after a stop/restart race.
    audio_generations: HashMap<Uuid, u64>,
    pending_triggers: Vec<PendingTrigger>,
    /// Local date + configured clock value used to deduplicate each daily time trigger.
    time_trigger_last_fired: HashMap<Uuid, String>,
    active_playlists: Vec<ActivePlaylist>,
    active_all_cues: Vec<ActiveAllCues>,
    startup_frame_logged: bool,
    last_backup_at: Instant,
    last_media_scan_at: Instant,
}

impl TheatreShowCueApp {
    pub fn new(cc: &eframe::CreationContext) -> Self {
        let mut settings = AppSettings::load();
        let requested_backend = settings.audio_backend;
        let requested_performance = AudioPerformanceSettings {
            sample_rate: settings.audio_sample_rate,
            buffer_size_frames: settings.audio_buffer_size_frames,
        };
        Theme::apply(&cc.egui_ctx, &settings.appearance);
        let mut audio = match AudioEngine::new_with_backend_and_endpoint_and_performance(
            settings.audio_backend,
            settings.audio_output_device_id.as_deref(),
            settings.audio_output_device.as_deref(),
            requested_performance,
        ) {
            Ok(audio) => audio,
            Err(error) => {
                warn!(
                    "Could not initialise saved audio endpoint '{} — {}' with performance settings {:?}: {}. Retrying the same endpoint with backend defaults.",
                    settings.audio_backend.short_label(),
                    settings.audio_output_device.as_deref().unwrap_or("System Default"),
                    requested_performance,
                    error
                );
                match AudioEngine::new_with_backend_and_endpoint_and_performance(
                    settings.audio_backend,
                    settings.audio_output_device_id.as_deref(),
                    settings.audio_output_device.as_deref(),
                    AudioPerformanceSettings::default(),
                ) {
                    Ok(audio) => {
                        settings.audio_sample_rate = None;
                        settings.audio_buffer_size_frames = None;
                        let _ = settings.save();
                        audio
                    }
                    Err(backend_error) => {
                        warn!(
                            "Could not initialise {} backend: {}. Falling back to WASAPI/system default.",
                            settings.audio_backend.label(),
                            backend_error
                        );
                        settings.audio_backend = AudioBackend::Wasapi;
                        settings.audio_output_device = None;
                        settings.audio_output_device_id = None;
                        settings.audio_sample_rate = None;
                        settings.audio_buffer_size_frames = None;
                        AudioEngine::new().unwrap_or_else(|fallback_error| {
                            panic!("Cannot initialise audio engine: {fallback_error}");
                        })
                    }
                }
            }
        };
        if let Some(device) = settings.audio_input_device.as_deref() {
            if audio.input_devices.iter().any(|name| name == device) {
                audio.set_input_device(device);
            }
        }
        let logical_pairs_migrated = migrate_saved_stereo_pairs_to_logical(&mut settings);
        audio.set_output_channel_names(settings.output_channel_names.clone());
        audio.set_default_output_pairs(settings.default_output_pairs.clone());
        audio.set_output_channel_orders(settings.output_channel_orders.clone());
        if logical_pairs_migrated {
            let _ = settings.save();
        }
        settings.output_channels.resize(
            audio.output_channels.max(1),
            crate::cue::OutputChannelSetting::default(),
        );
        settings
            .output_channels
            .truncate(audio.output_channels.max(1));
        audio.set_output_channel_settings(settings.output_channels.clone());
        for device in audio.output_devices.clone() {
            let settings_key = audio.output_device_settings_key(&device);
            if let Some(device_settings) = settings
                .output_channel_settings_by_device
                .get(&settings_key)
            {
                audio.set_output_channel_settings_for_device(&device, device_settings.clone());
            }
        }
        settings.output_channels = audio.output_channel_settings();
        if let Some(active_directory) = crate::diagnostics::active_log_directory() {
            let saved_directory = settings
                .diagnostic_log_directory
                .as_deref()
                .map(PathBuf::from);
            if saved_directory
                .as_ref()
                .is_some_and(|saved| saved != &active_directory)
            {
                // The configured path was unavailable at startup; keep the
                // safe fallback rather than persisting an unusable location.
                settings.diagnostic_log_directory = None;
                let _ = settings.save();
            }
        }
        if let Err(error) = crate::diagnostics::write_runtime_report(
            audio.backend.label(),
            &audio.output_devices,
            &audio.input_devices,
            audio.output_channels,
        ) {
            warn!("Could not write diagnostic report: {error}");
        }

        if settings.audio_backend != requested_backend
            || settings.audio_output_device.as_deref() != Some(audio.current_device.as_str())
            || settings.audio_output_device_id.as_deref() != Some(audio.current_device_id.as_str())
        {
            settings.audio_backend = audio.backend;
            settings.audio_output_device = Some(audio.current_device.clone());
            settings.audio_output_device_id = Some(audio.current_device_id.clone());
            let _ = settings.save();
        }

        let device_registry_monitor = DeviceRegistryMonitor::spawn(
            AudioEngine::scan_hotplug_output_endpoints,
            Duration::from_secs(5),
        );

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
        info!("Theatre Show Cue initialization complete; creating the main window");

        Self {
            audio,
            device_registry_monitor,
            active_output_needs_reopen: false,
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
            scroll_to_cue: None,
            selected_ids: HashSet::new(),
            selection_anchor: None,
            cue_clipboard: Vec::new(),
            editing: None,
            editing_focus_pending: false,
            project_name_edit: None,
            project_name_edit_focus_pending: false,
            expanded_groups: HashSet::new(),
            active_panel: Panel::Inspector,
            master_vol: 1.0,
            status_msg: "Ready — drag audio files here or add events from the toolbar".to_string(),
            pending_triggers: Vec::new(),
            time_trigger_last_fired: HashMap::new(),
            active_playlists: Vec::new(),
            active_all_cues: Vec::new(),
            startup_frame_logged: false,
            last_backup_at: Instant::now(),
            last_media_scan_at: Instant::now(),
            cue_column_widths,
            bottom_panel_height,
            bottom_panel_visible: true,
            bottom_panel_resize_active: false,
            bottom_panel_resize_release_guard: false,
            notes_sidebar_open: true,
            fade_started_at: HashMap::new(),
            fade_managed_targets: HashSet::new(),
            audio_generations: HashMap::new(),
        }
    }

    // ── Запуск и диспетчеризация всех типов событий ──────────────────────────

    fn go(&mut self) {
        // A Comment is a passive marker. GO on a mouse-selected comment only
        // advances the operator cursor; it must never execute the following cue.
        if let Some(selected_id) = self.selected {
            if self.is_comment_cue(selected_id) {
                if let Some(next_id) = self.next_non_comment_after(selected_id) {
                    self.select_only(next_id);
                }
                return;
            }
        }

        let id = match self.selected {
            Some(id) => id,
            None => {
                let visible =
                    visible_navigation_cue_ids(&self.cue_list.cues, &self.expanded_groups);
                if let Some(first) = visible.into_iter().next() {
                    first
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
        // If there is no next cue for a manual GO, clear the operator cursor:
        // the playing cue remains visibly marked by its playback outline.
        if self.selected == Some(id) {
            if let Some(next_id) = next_manual {
                self.select_only(next_id);
            } else {
                self.selected = None;
                self.selected_ids.clear();
                self.selection_anchor = None;
                self.editing = None;
                self.editing_focus_pending = false;
            }
        }
    }

    fn selection_after_auto_chain(&self, start_id: Uuid) -> Option<Uuid> {
        let mut current_id = start_id;
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current_id) {
                // An automatic cycle has no next manual cue to select.
                return None;
            }
            let Some(cue) = self.cue_list.find(current_id) else {
                return None;
            };
            // Comments are transparent to automatic sequence navigation.
            if matches!(&cue.cue_type, CueType::Comment { .. }) {
                current_id = self.next_cue_id(current_id)?;
                continue;
            }
            if cue.continue_mode == ContinueMode::Nothing {
                return self.next_non_comment_after(current_id);
            }
            let Some(next_id) = self.next_auto_cue_id(current_id) else {
                return None;
            };
            current_id = next_id;
        }
    }

    fn is_comment_cue(&self, id: Uuid) -> bool {
        self.cue_list
            .find(id)
            .map(|cue| matches!(&cue.cue_type, CueType::Comment { .. }))
            .unwrap_or(false)
    }

    /// Find the next item in the cue sequence, skipping passive comment rows.
    fn next_non_comment_after(&self, current_id: Uuid) -> Option<Uuid> {
        let mut candidate = self.next_cue_id(current_id)?;
        let mut visited = HashSet::new();
        while visited.insert(candidate) {
            if !self.is_comment_cue(candidate) {
                return Some(candidate);
            }
            candidate = self.next_cue_id(candidate)?;
        }
        None
    }

    fn trigger_cue(&mut self, id: Uuid) {
        let is_comment = self.is_comment_cue(id);
        if self.cue_list.find(id).is_none() {
            return;
        }

        // Comment cues are pass-through markers for automatic chains and groups.
        // Manual GO handles them separately above so it never starts the next cue.
        if is_comment {
            if let Some(next_id) = self.next_auto_cue_id(id) {
                self.select_only(next_id);
                self.trigger_cue(next_id);
            }
            return;
        }

        // GO on an already-running cue is a fresh trigger. Remove stale waits,
        // completion callbacks and continuation callbacks from its previous run.
        self.pending_triggers
            .retain(|trigger| trigger.cue_id != id && trigger.source_cue_id != Some(id));
        self.clear_fade_managed_targets_for_cue(id);
        self.fade_started_at.remove(&id);
        self.fade_managed_targets.remove(&id);

        let Some(cue) = self.cue_list.find(id) else {
            return;
        };
        info!(
            "Cue trigger requested: id={}, number='{}', name='{}', state={:?}, pre_wait={:.3}s, post_wait={:.3}s",
            cue.id,
            cue.number,
            cue.name,
            cue.state,
            cue.pre_wait_secs,
            cue.post_wait_secs
        );
        let pre_wait = cue.pre_wait_secs.max(0.0);
        let post_wait = cue.post_wait_secs.max(0.0);
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
            let wait_started_at = Instant::now();
            if let Some(cue) = self.cue_list.find_mut(id) {
                cue.state = CueState::Waiting;
                cue.position_secs = 0.0;
                cue.wait_phase = WaitPhase::Pre;
                cue.wait_started_at = Some(wait_started_at);
                cue.wait_duration_secs = pre_wait;
            }
            self.pending_triggers.push(PendingTrigger {
                cue_id: id,
                source_cue_id: Some(id),
                fire_at: wait_started_at + std::time::Duration::from_secs_f64(pre_wait),
                action: PendingAction::ExecuteCue,
            });
            self.status_msg = format!("⏳ Pre-wait ({:.1}s): {}", pre_wait, cue_name);
        } else {
            self.execute_cue_now(id);
        }

        // Auto-Continue keeps its time-based behaviour in Normal mode. Playlist
        // and All Cues own their child order, so their children cannot schedule
        // a second, competing continuation chain.
        if continue_mode == ContinueMode::AutoContinue && !is_managed_child && !is_group_run {
            if let Some(next_id) = self.next_auto_cue_id(id) {
                let fade_target_is_next = self.fade_target_is_next_or_container(id, next_id);
                if fade_target_is_next {
                    // Fade already started/operates this next cue. Let that cue's
                    // own Continue setting control what follows it.
                    return;
                }
                let now = Instant::now();
                let delay = pre_wait + post_wait;
                if post_wait > 0.0 {
                    self.pending_triggers.push(PendingTrigger {
                        cue_id: id,
                        source_cue_id: Some(id),
                        fire_at: now + std::time::Duration::from_secs_f64(pre_wait),
                        action: PendingAction::BeginPostWait,
                    });
                    self.pending_triggers.push(PendingTrigger {
                        cue_id: id,
                        source_cue_id: Some(id),
                        fire_at: now + std::time::Duration::from_secs_f64(delay),
                        action: PendingAction::ClearWait,
                    });
                }
                if delay > 0.0 {
                    self.pending_triggers.push(PendingTrigger {
                        cue_id: next_id,
                        source_cue_id: Some(id),
                        fire_at: now + std::time::Duration::from_secs_f64(delay),
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
        let now = Instant::now();
        let extra_delay = extra_delay.max(0.0);

        // Auto-Continue can already be counting the cue's post-wait while an
        // audio cue is still playing. If playback ends during that countdown,
        // reuse the remaining time instead of restarting the post-wait clock.
        let existing_post_wait_remaining = self.cue_list.find(cue_id).and_then(|cue| {
            if cue.wait_phase != WaitPhase::Post {
                return None;
            }
            let started_at = cue.wait_started_at?;
            if now < started_at {
                return None;
            }
            Some((cue.wait_duration_secs - now.duration_since(started_at).as_secs_f64()).max(0.0))
        });

        let delay = if let Some(remaining) = existing_post_wait_remaining {
            remaining
        } else {
            if post_wait > 0.0 {
                if let Some(cue) = self.cue_list.find_mut(cue_id) {
                    cue.wait_phase = WaitPhase::Post;
                    cue.wait_started_at =
                        Some(now + std::time::Duration::from_secs_f64(extra_delay));
                    cue.wait_duration_secs = post_wait;
                    if extra_delay <= 0.0 {
                        cue.position_secs = 0.0;
                        if !matches!(
                            &cue.state,
                            CueState::Playing
                                | CueState::FadingIn
                                | CueState::FadingOut
                                | CueState::Paused
                        ) {
                            cue.state = CueState::Waiting;
                        }
                    }
                }
            }
            extra_delay + post_wait
        };

        if delay > 0.0 {
            self.pending_triggers.push(PendingTrigger {
                cue_id,
                source_cue_id: Some(cue_id),
                fire_at: now + std::time::Duration::from_secs_f64(delay),
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
            cue.state = CueState::Stopped;
            cue.position_secs = 0.0;
            cue.clear_wait_runtime();
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
                        source_cue_id: Some(cue_id),
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
                if self.fade_target_is_next_or_container(cue_id, next_id) {
                    // The target is already the next logical cue and was started
                    // by this Fade. Do not restart it or jump over its Continue=Nothing.
                    return;
                }
                self.trigger_cue(next_id);
            }
        }
    }

    fn cue_play_start_position(cue: &Cue) -> f64 {
        // GO on an active cue is a restart, not a seek-resume. A stopped cue may
        // still carry a deliberate waveform start position selected by the user.
        let position = if cue.is_active() {
            cue.start_secs
        } else if cue.position_secs > cue.start_secs {
            cue.position_secs
        } else {
            cue.start_secs
        };
        position.clamp(cue.start_secs.max(0.0), cue.effective_end_secs())
    }

    fn cue_output_backend(&self, cue: &Cue) -> AudioBackend {
        cue.io
            .output_backend
            .or_else(|| {
                cue.io
                    .output_device_id
                    .as_deref()
                    .and_then(|id| self.audio.device_registry.find_by_id(id))
                    .map(|endpoint| endpoint.backend)
            })
            .unwrap_or(self.audio.backend)
    }

    /// Resolve a saved Cue endpoint by stable ID. For older projects, migrate
    /// the legacy (backend, display name) pair to the registry ID when available.
    fn normalize_cue_output_endpoint(&mut self, cue_id: Uuid, cue: &mut Cue) {
        let Some(device_name) = cue.io.output_device.clone() else {
            return;
        };
        let route_backend = self.cue_output_backend(cue);

        let endpoint = if let Some(saved_id) = cue.io.output_device_id.as_deref() {
            self.audio
                .device_registry
                .find_available_by_id(saved_id)
                .filter(|endpoint| endpoint.backend == route_backend)
                .cloned()
        } else {
            self.audio
                .device_registry
                .find_available_by_backend_name(route_backend, &device_name)
                .cloned()
        };

        if let Some(endpoint) = endpoint {
            if endpoint.backend == self.audio.backend {
                self.audio.refresh_output_device_info(&endpoint.device_name);
            }
            cue.io.output_device = Some(endpoint.device_name.clone());
            cue.io.output_device_id = Some(endpoint.id.clone());
            cue.io.output_backend = Some(endpoint.backend);
            if let Some(stored_cue) = self.cue_list.find_mut(cue_id) {
                stored_cue.io.output_device = Some(endpoint.device_name);
                stored_cue.io.output_device_id = Some(endpoint.id);
                stored_cue.io.output_backend = Some(endpoint.backend);
            }
        }
    }

    /// Returns a short reason when an Audio Cue currently has no usable output.
    /// A missing `output_device` is not an error: it intentionally means the
    /// active global output. Explicit offline/cross-backend routes and empty
    /// output matrices are surfaced in the Cue List.
    fn cue_output_issue_for_table(&self, cue: &Cue) -> Option<String> {
        if !cue_owns_audio_stream(cue) {
            return None;
        }

        let route_backend = self.cue_output_backend(cue);
        let saved_device_name = cue.io.output_device.as_deref();
        let has_explicit_endpoint =
            saved_device_name.is_some() || cue.io.output_device_id.is_some();
        let device_name = cue
            .io
            .output_device_id
            .as_deref()
            .and_then(|id| self.audio.device_registry.find_by_id(id))
            .map(|endpoint| endpoint.device_name.as_str())
            .or(saved_device_name)
            .unwrap_or(self.audio.current_device.as_str());
        let output_device = has_explicit_endpoint.then_some(device_name);

        if has_explicit_endpoint {
            let is_current_endpoint = route_backend == self.audio.backend
                && device_name == self.audio.current_device.as_str();
            let endpoint_unavailable = if is_current_endpoint {
                self.audio
                    .device_registry
                    .find_by_id(&self.audio.current_device_id)
                    .is_some_and(|endpoint| !endpoint.is_available())
            } else {
                !self.audio.registered_endpoint_available(
                    route_backend,
                    cue.io.output_device_id.as_deref(),
                    device_name,
                )
            };
            if endpoint_unavailable {
                return Some(format!(
                    "{} — '{}' is unavailable or disconnected.",
                    route_backend.short_label(),
                    device_name
                ));
            }

            if route_backend != self.audio.backend {
                return Some(format!(
                    "{} — '{}' cannot be used while {} is the active backend. Cross-backend playback is not available yet.",
                    route_backend.short_label(),
                    device_name,
                    self.audio.backend.short_label()
                ));
            }

            if route_backend == AudioBackend::Asio
                && device_name != self.audio.current_device.as_str()
            {
                return Some(format!(
                    "ASIO — '{}' is not the active ASIO driver. Only '{}' can play until multi-ASIO workers are implemented.",
                    device_name,
                    self.audio.current_device
                ));
            }
        } else if route_backend != self.audio.backend {
            return Some(format!(
                "The global output is configured for {}, but {} is active.",
                route_backend.short_label(),
                self.audio.backend.short_label()
            ));
        } else if self
            .audio
            .device_registry
            .find_by_id(&self.audio.current_device_id)
            .is_some_and(|endpoint| !endpoint.is_available())
        {
            return Some(format!(
                "The global output endpoint '{}' is unavailable or disconnected.",
                self.audio.current_device
            ));
        } else if self.audio.current_sample_rate == 0 || self.audio.output_channels == 0 {
            return Some("The global audio output stream is not active.".to_string());
        }

        let output_count = self.audio.output_channels_for_device(output_device);
        let output_settings = self.audio.output_channel_settings_for_device(output_device);
        let output_order = self.audio.output_channel_display_order(output_device);
        let has_active_destination = cue.io.channel_routes.iter().any(|route| {
            if !route.active {
                return false;
            }
            let destinations: Vec<usize> = if route.output_channels.is_empty() {
                vec![route.output_channel]
            } else {
                route.output_channels.clone()
            };
            destinations.iter().any(|index| {
                *index < output_count
                    && output_settings
                        .get(output_order.get(*index).copied().unwrap_or(*index))
                        .map(|setting| setting.active)
                        .unwrap_or(true)
            })
        });

        if !has_active_destination
            && (cue.audio_channels > 0
                || !cue.io.channel_routes.is_empty()
                || !cue.file_path.as_os_str().is_empty())
        {
            return Some("No active hardware output is assigned to this Cue.".to_string());
        }

        None
    }

    fn cue_output_issues_for_table(&self) -> HashMap<Uuid, String> {
        self.cue_list
            .cues
            .iter()
            .filter_map(|cue| {
                self.cue_output_issue_for_table(cue)
                    .map(|reason| (cue.id, reason))
            })
            .collect()
    }

    /// Do not start a Cue if its saved endpoint is currently offline or belongs
    /// to a backend/ASIO driver that this Stage 3 process cannot service.
    fn cue_output_backend_is_active(&mut self, cue: &Cue) -> bool {
        let Some(device_name) = cue.io.output_device.as_deref() else {
            if self
                .audio
                .device_registry
                .find_available_by_id(&self.audio.current_device_id)
                .is_some()
            {
                return true;
            }
            let message = format!(
                "Cue '{}' uses the global output {} — '{}', but that endpoint is currently unavailable.",
                cue.name,
                self.audio.backend.short_label(),
                self.audio.current_device
            );
            self.status_msg = message.clone();
            warn!("{}", message);
            return false;
        };
        let route_backend = self.cue_output_backend(cue);

        if !self.audio.registered_endpoint_available(
            route_backend,
            cue.io.output_device_id.as_deref(),
            device_name,
        ) {
            let message = format!(
                "Cue '{}' is routed to {} — '{}', but that endpoint is currently unavailable. The saved route has been retained; reconnect the device and refresh the registry.",
                cue.name,
                route_backend.short_label(),
                device_name
            );
            self.status_msg = message.clone();
            warn!("{}", message);
            return false;
        }

        if route_backend != self.audio.backend {
            let message = format!(
                "Cue '{}' is routed to {} — '{}', but the active backend is {}. Cross-backend playback will be enabled in Stage 4.",
                cue.name,
                route_backend.short_label(),
                device_name,
                self.audio.backend.short_label()
            );
            self.status_msg = message.clone();
            warn!("{}", message);
            return false;
        }

        if route_backend == AudioBackend::Asio && device_name != self.audio.current_device.as_str()
        {
            let message = format!(
                "Cue '{}' is routed to ASIO — '{}', but '{}' is the active ASIO driver. Only one ASIO driver can be active in this process; select the Cue's device as the global output or wait for Stage 4.",
                cue.name,
                device_name,
                self.audio.current_device
            );
            self.status_msg = message.clone();
            warn!("{}", message);
            return false;
        }

        true
    }

    /// Start an Audio cue. An optional override is used by a Fade In cue when
    /// its target is currently stopped, so the track starts from silence and
    /// reaches the requested level using the selected fade curve.
    fn start_audio_cue(
        &mut self,
        id: Uuid,
        fade_override: Option<(f64, FadeCurve, Vec<[f32; 2]>, f32)>,
    ) -> bool {
        // Fade In can start its target directly, so center that playing audio cue too.
        self.scroll_to_cue = Some(id);
        let Some(mut snapshot) = self.cue_list.find(id).cloned() else {
            return false;
        };
        if !matches!(&snapshot.cue_type, CueType::Audio) {
            return false;
        }
        self.normalize_cue_output_endpoint(id, &mut snapshot);
        if !self.cue_output_backend_is_active(&snapshot) {
            return false;
        }

        let source_channels = snapshot.audio_channels.max(1);
        let output_channels = self
            .audio
            .output_channels_for_device(snapshot.io.output_device.as_deref());
        let mut routing = snapshot.io.clone();
        let default_route_channels = self.audio.default_route_channels_for_source(
            snapshot.io.output_device.as_deref(),
            source_channels,
        );
        routing.ensure_channel_routes_with_defaults(
            source_channels,
            output_channels,
            &default_route_channels,
        );
        if let Some(cue) = self.cue_list.find_mut(id) {
            cue.state = CueState::Playing;
            cue.audio_channels = source_channels;
            cue.io.channel_routes = routing.channel_routes.clone();
            cue.io.output_gains_db = routing.output_gains_db.clone();
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

        let native_routes = self.audio.cue_routes_for_device(
            snapshot.io.output_device.as_deref(),
            &routing.channel_routes,
        );
        let native_gains = self.audio.cue_gains_for_device(
            snapshot.io.output_device.as_deref(),
            &routing.output_gains_db,
        );
        info!(
            "Audio Cue routing: id={}, logical_routes={:?}, native_routes={:?}, mapping={:?}",
            snapshot.id,
            routing.channel_routes,
            native_routes,
            self.audio
                .output_channel_display_order(snapshot.io.output_device.as_deref()),
        );
        let generation = self.bump_audio_generation(snapshot.id);
        self.audio.send(AudioCommand::Play {
            id: snapshot.id,
            generation,
            file: snapshot.file_path.clone(),
            output_device: snapshot.io.output_device.clone(),
            start_secs: Self::cue_play_start_position(&snapshot),
            end_secs: snapshot.end_secs,
            duration_secs: snapshot.duration_secs,
            volume,
            volume_automation_enabled: snapshot.volume_automation_enabled,
            volume_automation: snapshot.volume_automation.clone(),
            time_pitch: snapshot.time_pitch.clone(),
            fade_in_secs,
            fade_out_secs: snapshot.fade_out_secs,
            fade_curve,
            custom_fade_points,
            channel_routes: native_routes,
            output_gains_db: native_gains,
            loop_enabled: snapshot.loop_region.enabled,
            loop_start: snapshot.loop_region.start_secs,
            loop_end: snapshot.loop_region.end_secs,
            loop_count: snapshot.loop_region.count,
        });
        true
    }

    fn execute_cue_now(&mut self, id: Uuid) {
        // Keep the cue that actually starts execution centered in the Cue List.
        // This also covers Auto-Continue/Auto-Follow and delayed pre-wait cues.
        self.scroll_to_cue = Some(id);
        if let Some(cue) = self.cue_list.find_mut(id) {
            cue.clear_wait_runtime();
            if cue.state == CueState::Waiting {
                cue.state = CueState::Stopped;
                cue.position_secs = 0.0;
            }
        }
        let Some(mut cue_snapshot) = self.cue_list.find(id).cloned() else {
            return;
        };
        self.normalize_cue_output_endpoint(id, &mut cue_snapshot);
        info!(
            "Cue execution started: id={}, number='{}', name='{}'",
            cue_snapshot.id, cue_snapshot.number, cue_snapshot.name
        );

        let cue_starts_audio_directly = matches!(
            &cue_snapshot.cue_type,
            CueType::Audio | CueType::Play { target_id: None }
        ) && (matches!(&cue_snapshot.cue_type, CueType::Audio)
            || !cue_snapshot.file_path.as_os_str().is_empty());
        if cue_starts_audio_directly && !self.cue_output_backend_is_active(&cue_snapshot) {
            if let Some(cue) = self.cue_list.find_mut(id) {
                cue.state = CueState::Stopped;
            }
            return;
        }

        if matches!(&cue_snapshot.cue_type, CueType::Group { .. }) {
            let descendants = self.group_descendant_ids(id);
            let has_active_child = descendants.iter().skip(1).any(|child_id| {
                self.cue_list
                    .find(*child_id)
                    .map(|child| child.is_active())
                    .unwrap_or(false)
            });
            let has_pending_child = self.pending_triggers.iter().any(|trigger| {
                descendants.contains(&trigger.cue_id)
                    || trigger
                        .source_cue_id
                        .is_some_and(|source_id| descendants.contains(&source_id))
            });
            let has_group_run = self.active_playlists.iter().any(|run| run.group_id == id)
                || self.active_all_cues.iter().any(|run| run.group_id == id);
            if has_active_child || has_pending_child || has_group_run {
                // Re-triggering a group starts a clean run from its first child.
                self.stop_target_and_descendants(id);
            }
        }

        match cue_snapshot.cue_type {
            CueType::Audio => {
                if !self.start_audio_cue(id, None) {
                    return;
                }
                self.report
                    .record(&cue_snapshot.number, &cue_snapshot.name, 0.0, "Play");
                self.status_msg = format!("▶ Playing: {}", cue_snapshot.name);
            }

            CueType::Play { target_id } => {
                let mut started_audio = false;
                if let Some(tid) = target_id {
                    self.execute_cue_now(tid);
                } else if !cue_snapshot.file_path.as_os_str().is_empty() {
                    started_audio = true;
                    let source_channels = cue_snapshot.audio_channels.max(1);
                    let track_output_channels = self
                        .audio
                        .output_channels_for_device(cue_snapshot.io.output_device.as_deref());
                    let mut io_routing = cue_snapshot.io.clone();
                    let default_route_channels = self.audio.default_route_channels_for_source(
                        cue_snapshot.io.output_device.as_deref(),
                        source_channels,
                    );
                    io_routing.ensure_channel_routes_with_defaults(
                        source_channels,
                        track_output_channels,
                        &default_route_channels,
                    );
                    if let Some(cue) = self.cue_list.find_mut(id) {
                        cue.state = CueState::Playing;
                        cue.audio_channels = source_channels;
                        cue.io.channel_routes = io_routing.channel_routes.clone();
                        cue.io.output_gains_db = io_routing.output_gains_db.clone();
                    }
                    let native_routes = self.audio.cue_routes_for_device(
                        cue_snapshot.io.output_device.as_deref(),
                        &io_routing.channel_routes,
                    );
                    let native_gains = self.audio.cue_gains_for_device(
                        cue_snapshot.io.output_device.as_deref(),
                        &io_routing.output_gains_db,
                    );
                    info!(
                        "Play Cue routing: id={}, logical_routes={:?}, native_routes={:?}, mapping={:?}",
                        cue_snapshot.id,
                        io_routing.channel_routes,
                        native_routes,
                        self.audio.output_channel_display_order(cue_snapshot.io.output_device.as_deref()),
                    );
                    let generation = self.bump_audio_generation(cue_snapshot.id);
                    self.audio.send(AudioCommand::Play {
                        id: cue_snapshot.id,
                        generation,
                        file: cue_snapshot.file_path.clone(),
                        output_device: cue_snapshot.io.output_device.clone(),
                        start_secs: Self::cue_play_start_position(&cue_snapshot),
                        end_secs: cue_snapshot.end_secs,
                        duration_secs: cue_snapshot.duration_secs,
                        volume: cue_snapshot.volume * self.master_vol,
                        volume_automation_enabled: cue_snapshot.volume_automation_enabled,
                        volume_automation: cue_snapshot.volume_automation.clone(),
                        time_pitch: cue_snapshot.time_pitch.clone(),
                        fade_in_secs: cue_snapshot.fade_in_secs,
                        fade_out_secs: cue_snapshot.fade_out_secs,
                        fade_curve: cue_snapshot.fade_curve,
                        custom_fade_points: cue_snapshot.custom_fade_points.clone(),
                        channel_routes: native_routes,
                        output_gains_db: native_gains,
                        loop_enabled: cue_snapshot.loop_region.enabled,
                        loop_start: cue_snapshot.loop_region.start_secs,
                        loop_end: cue_snapshot.loop_region.end_secs,
                        loop_count: cue_snapshot.loop_region.count,
                    });
                }
                if !started_audio {
                    self.schedule_cue_completion(id, 0.0);
                }
            }

            CueType::Pause { target_id } => {
                if let Some(tid) = target_id {
                    let affected = self.group_descendant_ids(tid);
                    let audio_targets = self.audio_targets_in(tid);
                    self.pending_triggers.retain(|trigger| {
                        !affected.contains(&trigger.cue_id)
                            && !trigger
                                .source_cue_id
                                .is_some_and(|source_id| affected.contains(&source_id))
                    });

                    // Cancel any pending pre/post-wait on the target tree. Active
                    // audio players are paused; cues that were only waiting are
                    // returned to Stopped so they cannot fire later unexpectedly.
                    for affected_id in &affected {
                        if let Some(target) = self.cue_list.find_mut(*affected_id) {
                            let was_waiting = target.state == CueState::Waiting;
                            target.clear_wait_runtime();
                            if was_waiting {
                                target.state = CueState::Stopped;
                                target.position_secs = 0.0;
                            }
                        }
                    }
                    for audio_id in audio_targets {
                        let should_pause = self
                            .cue_list
                            .find(audio_id)
                            .map(|target| target.is_active() && target.state != CueState::Waiting)
                            .unwrap_or(false);
                        if let Some(target) = self.cue_list.find_mut(audio_id) {
                            if should_pause {
                                target.state = CueState::Paused;
                            } else {
                                target.state = CueState::Stopped;
                                target.position_secs = 0.0;
                            }
                        }
                        if should_pause {
                            self.audio.send(AudioCommand::Pause(audio_id));
                        } else {
                            self.bump_audio_generation(audio_id);
                            self.audio.send(AudioCommand::Stop(audio_id));
                        }
                    }
                    if let Some(target) = self.cue_list.find_mut(tid) {
                        if target.state != CueState::Stopped {
                            target.state = CueState::Paused;
                        }
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
                let targets = if let Some(target_id) = target_id {
                    self.audio_targets_in(target_id)
                } else {
                    // "None / All Active" means all currently active audio tracks.
                    self.cue_list
                        .cues
                        .iter()
                        .filter(|cue| cue_owns_audio_stream(cue) && cue.is_active())
                        .map(|cue| cue.id)
                        .collect()
                };

                let mut fade_started = false;
                let mut fade_start_blocked = false;
                let next_id = self.next_auto_cue_id(id);
                let target_is_next = next_id
                    .map(|next_id| self.fade_target_is_next_or_container(id, next_id))
                    .unwrap_or(false);

                for audio_id in targets {
                    let Some(target) = self.cue_list.find(audio_id).cloned() else {
                        continue;
                    };
                    let target_vol = target_volume.unwrap_or_else(|| {
                        if fade_in {
                            if target.volume <= 0.000_001 {
                                1.0
                            } else {
                                target.volume
                            }
                        } else {
                            0.0
                        }
                    }) * self.master_vol;

                    let stopped = matches!(&target.state, CueState::Stopped | CueState::Error(_));
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
                        fade_start_blocked = true;
                        continue;
                    }

                    // If the target is the next row, it owns its own continuation.
                    // Otherwise this Fade owns the continuation chain and suppresses
                    // the target's independent auto-follow when it eventually ends.
                    let fade_owns_continue =
                        cue_snapshot.continue_mode != ContinueMode::Nothing && !target_is_next;
                    let suppress_target_continue =
                        fade_owns_continue || (!fade_in && stop_on_finish && !target_is_next);
                    if suppress_target_continue {
                        self.fade_managed_targets.insert(audio_id);
                    } else {
                        self.fade_managed_targets.remove(&audio_id);
                    }

                    if fade_in && stopped {
                        if self.start_audio_cue(
                            audio_id,
                            Some((
                                duration_secs,
                                fade_curve,
                                custom_fade_points.clone(),
                                target_vol,
                            )),
                        ) {
                            if let Some(target) = self.cue_list.find_mut(audio_id) {
                                target.state = CueState::FadingIn;
                            }
                            fade_started = true;
                        } else {
                            fade_start_blocked = true;
                            continue;
                        }
                    } else if playing || (fade_in && paused) {
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

                // A Fade cue remains a timed cue even when its target is already
                // stopped (Fade Out) or no active target exists. Its own Continue
                // behaviour must still be honoured. The only blocked case is a
                // Fade In explicitly targeting a stopped cue with Continue=Nothing.
                let should_run_timer = fade_started || !fade_start_blocked;
                if should_run_timer {
                    if let Some(fade_cue) = self.cue_list.find_mut(id) {
                        fade_cue.state = if fade_in {
                            CueState::FadingIn
                        } else {
                            CueState::FadingOut
                        };
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
                    // Auto-Follow is handled by complete_cue after duration + post-wait.
                    // Auto-Continue is handled by trigger_cue after pre/post-wait,
                    // independently of the Fade duration.
                    self.schedule_cue_completion(id, duration_secs);
                } else {
                    self.fade_started_at.remove(&id);
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

            CueType::Comment { .. } => {
                // Comment cues are intentionally non-executable. The trigger_cue
                // guard treats them as transparent markers in automatic chains.
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
                        if let Some(first_id) = children.iter().copied().find(|child_id| {
                            self.cue_list.find(*child_id).is_some()
                                && !self.is_comment_cue(*child_id)
                        }) {
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
                        let playable_children: Vec<Uuid> = children
                            .into_iter()
                            .filter(|child_id| {
                                self.cue_list.find(*child_id).is_some()
                                    && !self.is_comment_cue(*child_id)
                            })
                            .collect();
                        if playable_children.is_empty() {
                            self.status_msg = "📁 Playlist is empty".to_string();
                            self.schedule_cue_completion(id, 0.0);
                        } else {
                            self.active_playlists.retain(|run| run.group_id != id);
                            self.active_playlists.push(ActivePlaylist {
                                group_id: id,
                                children: playable_children.clone(),
                                current_idx: 0,
                                fade_in_secs,
                                fade_out_secs,
                            });
                            self.start_playlist_child(
                                id,
                                playable_children,
                                0,
                                fade_in_secs,
                                fade_out_secs,
                            );
                        }
                    }
                    GroupMode::AllCues => {
                        let valid_children: Vec<Uuid> = children
                            .into_iter()
                            .filter(|child_id| {
                                self.cue_list.find(*child_id).is_some()
                                    && !self.is_comment_cue(*child_id)
                            })
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
                    .map(cue_owns_audio_stream)
                    .unwrap_or(false)
            })
            .collect()
    }

    fn clear_fade_managed_targets_for_cue(&mut self, fade_cue_id: Uuid) {
        let Some(cue) = self.cue_list.find(fade_cue_id) else {
            return;
        };
        let CueType::Fade { target_id, .. } = &cue.cue_type else {
            return;
        };
        let target_id = *target_id;
        let targets = if let Some(target_id) = target_id {
            self.audio_targets_in(target_id)
        } else {
            self.cue_list
                .cues
                .iter()
                .filter(|cue| cue_owns_audio_stream(cue) && cue.is_active())
                .map(|cue| cue.id)
                .collect()
        };
        for target_id in targets {
            self.fade_managed_targets.remove(&target_id);
        }
    }

    fn bump_audio_generation(&mut self, cue_id: Uuid) -> u64 {
        let generation = self.audio_generations.entry(cue_id).or_insert(0);
        *generation = generation.wrapping_add(1);
        *generation
    }

    fn stop_target_and_descendants(&mut self, target_id: Uuid) {
        let affected = self.group_descendant_ids(target_id);
        let fade_cues: Vec<Uuid> = affected
            .iter()
            .copied()
            .filter(|id| {
                self.cue_list
                    .find(*id)
                    .map(|cue| matches!(&cue.cue_type, CueType::Fade { .. }))
                    .unwrap_or(false)
            })
            .collect();
        for fade_id in fade_cues {
            self.clear_fade_managed_targets_for_cue(fade_id);
        }
        self.pending_triggers.retain(|trigger| {
            !affected.contains(&trigger.cue_id)
                && !trigger
                    .source_cue_id
                    .is_some_and(|source_id| affected.contains(&source_id))
        });
        self.active_playlists
            .retain(|run| !affected.contains(&run.group_id));
        self.active_all_cues
            .retain(|run| !affected.contains(&run.group_id));
        let audio_ids = self.audio_targets_in(target_id);
        for id in &affected {
            self.bump_audio_generation(*id);
            self.fade_started_at.remove(id);
            self.fade_managed_targets.remove(id);
        }
        for id in affected {
            if let Some(cue) = self.cue_list.find_mut(id) {
                cue.state = CueState::Stopped;
                cue.position_secs = 0.0;
                cue.clear_wait_runtime();
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
    /// Resolve the next non-comment cue in the visible project sequence.
    /// Fade-target collisions are handled separately so the target can act as
    /// a continuation barrier instead of being silently skipped.
    fn next_auto_cue_id(&self, current_id: Uuid) -> Option<Uuid> {
        let mut candidate = self.next_cue_id(current_id)?;
        let mut visited = HashSet::new();
        while visited.insert(candidate) {
            if !self.is_comment_cue(candidate) {
                return Some(candidate);
            }
            candidate = self.next_cue_id(candidate)?;
        }
        None
    }

    /// True when the next logical row is the Fade's target itself, or a group
    /// that contains that target. In that case the target has already been
    /// started/operated on by the Fade and must not be triggered a second time.
    fn fade_target_is_next_or_container(&self, fade_id: Uuid, next_id: Uuid) -> bool {
        let (target_id, fade_in) = self
            .cue_list
            .find(fade_id)
            .and_then(|cue| {
                if let CueType::Fade {
                    target_id, fade_in, ..
                } = &cue.cue_type
                {
                    Some((*target_id, *fade_in))
                } else {
                    None
                }
            })
            .unwrap_or((None, false));

        let Some(target_id) = target_id else {
            return false;
        };
        let target_is_active = self.audio_targets_in(target_id).iter().any(|audio_id| {
            self.cue_list
                .find(*audio_id)
                .map(|cue| cue.is_active())
                .unwrap_or(false)
        });

        // Fade In starts a stopped target as part of its operation. Fade Out
        // does not start a stopped target, so that row remains a valid next cue.
        let target_is_operated_on = fade_in || target_is_active;
        target_is_operated_on && (target_id == next_id || self.group_contains(next_id, target_id))
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

    fn begin_auto_continue_post_wait(&mut self, cue_id: Uuid) {
        let now = Instant::now();
        if let Some(cue) = self.cue_list.find_mut(cue_id) {
            let duration = cue.post_wait_secs.max(0.0);
            if duration <= 0.0 {
                return;
            }
            cue.wait_phase = WaitPhase::Post;
            cue.wait_started_at = Some(now);
            cue.wait_duration_secs = duration;
            if !matches!(
                &cue.state,
                CueState::Playing | CueState::FadingIn | CueState::FadingOut | CueState::Paused
            ) {
                cue.state = CueState::Waiting;
                cue.position_secs = 0.0;
            }
        }
    }

    fn clear_cue_wait(&mut self, cue_id: Uuid) {
        if let Some(cue) = self.cue_list.find_mut(cue_id) {
            cue.clear_wait_runtime();
            if cue.state == CueState::Waiting {
                cue.state = CueState::Stopped;
                cue.position_secs = 0.0;
            }
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
                PendingAction::BeginPostWait => self.begin_auto_continue_post_wait(trig.cue_id),
                PendingAction::ClearWait => self.clear_cue_wait(trig.cue_id),
                PendingAction::CompleteCue => self.complete_cue(trig.cue_id),
            }
        }
    }

    /// Fire enabled cue triggers once per local calendar day at the configured HH:MM or HH:MM:SS.
    fn poll_time_triggers(&mut self) {
        let now = chrono::Local::now();
        let today = now.format("%Y-%m-%d").to_string();
        let current_minute = now.format("%H:%M").to_string();
        let current_second = now.format("%H:%M:%S").to_string();

        let due: Vec<(Uuid, String, String)> =
            self.cue_list
                .cues
                .iter()
                .filter_map(|cue| {
                    let matches_clock =
                        if chrono::NaiveTime::parse_from_str(&cue.time_trigger_time, "%H:%M:%S")
                            .is_ok()
                        {
                            cue.time_trigger_time == current_second
                        } else {
                            chrono::NaiveTime::parse_from_str(&cue.time_trigger_time, "%H:%M")
                                .is_ok()
                                && cue.time_trigger_time == current_minute
                        };
                    let fire_key = format!("{today}|{}", cue.time_trigger_time);

                    if !cue.time_trigger_enabled
                        || matches!(&cue.cue_type, CueType::Comment { .. })
                        || !matches_clock
                        || self
                            .time_trigger_last_fired
                            .get(&cue.id)
                            .map(String::as_str)
                            == Some(fire_key.as_str())
                    {
                        return None;
                    }
                    Some((cue.id, cue.name.clone(), fire_key))
                })
                .collect();

        for (cue_id, cue_name, fire_key) in due {
            self.time_trigger_last_fired.insert(cue_id, fire_key);
            self.status_msg = format!("⏱ Time trigger: {cue_name}");
            self.trigger_cue(cue_id);
        }
    }

    // ── Transport actions ────────────────────────────────────────────────────

    fn stop_selected(&mut self) {
        let id = match self.selected {
            Some(id) => id,
            None => return,
        };
        if self.is_comment_cue(id) {
            return;
        }
        if let Some(cue) = self.cue_list.find(id) {
            info!(
                "Stop selected: id={}, number='{}', name='{}', position={:.3}s",
                cue.id, cue.number, cue.name, cue.position_secs
            );
        }
        self.clear_fade_managed_targets_for_cue(id);
        self.fade_started_at.remove(&id);
        self.fade_managed_targets.remove(&id);
        self.pending_triggers
            .retain(|trigger| trigger.cue_id != id && trigger.source_cue_id != Some(id));
        self.bump_audio_generation(id);
        if let Some(cue) = self.cue_list.find_mut(id) {
            self.report.record(
                &cue.number.clone(),
                &cue.name.clone(),
                cue.position_secs,
                "Stop",
            );
            cue.state = CueState::Stopped;
            cue.position_secs = 0.0;
            cue.clear_wait_runtime();
        }
        self.audio.send(AudioCommand::Stop(id));
        if self.is_managed_group_child(id) {
            self.schedule_cue_completion(id, 0.0);
        }
        self.status_msg = "⏹ Stopped".to_string();
    }

    fn stop_all(&mut self) {
        info!(
            "Stop all requested: cue_count={}, fade={:.3}s",
            self.cue_list.cues.len(),
            self.settings.stop_all_fade_secs.max(0.0)
        );
        self.pending_triggers.clear();
        self.active_playlists.clear();
        self.active_all_cues.clear();
        self.fade_started_at.clear();
        self.fade_managed_targets.clear();
        let fade_secs = self.settings.stop_all_fade_secs.max(0.0);
        if fade_secs > 0.0 {
            let immediate_stop_ids: Vec<Uuid> = self
                .cue_list
                .cues
                .iter()
                .filter(|cue| {
                    !(matches!(
                        &cue.state,
                        CueState::Playing | CueState::FadingIn | CueState::FadingOut
                    ) && matches!(&cue.cue_type, CueType::Audio))
                })
                .map(|cue| cue.id)
                .collect();
            for id in immediate_stop_ids {
                self.bump_audio_generation(id);
            }

            for cue in &mut self.cue_list.cues {
                cue.clear_wait_runtime();
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
                    if cue_owns_audio_stream(cue) {
                        self.audio.send(AudioCommand::Stop(cue.id));
                    }
                    cue.state = CueState::Stopped;
                    cue.position_secs = 0.0;
                    cue.clear_wait_runtime();
                }
            }
            self.status_msg = format!("Stopping all cues with {:.1}s fade", fade_secs);
        } else {
            let cue_ids: Vec<Uuid> = self.cue_list.cues.iter().map(|cue| cue.id).collect();
            for id in cue_ids {
                self.bump_audio_generation(id);
            }
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
        if self.is_comment_cue(id) {
            return;
        }
        let is_waiting = self
            .cue_list
            .find(id)
            .map(|cue| cue.state == CueState::Waiting)
            .unwrap_or(false);
        self.pending_triggers
            .retain(|trigger| trigger.cue_id != id && trigger.source_cue_id != Some(id));
        if is_waiting {
            if let Some(cue) = self.cue_list.find_mut(id) {
                cue.state = CueState::Stopped;
                cue.position_secs = 0.0;
                cue.clear_wait_runtime();
            }
            self.status_msg = "Wait cancelled".to_string();
            return;
        }
        if let Some(cue) = self.cue_list.find_mut(id) {
            info!(
                "Pause selected: id={}, number='{}', name='{}', position={:.3}s",
                cue.id, cue.number, cue.name, cue.position_secs
            );
            cue.clear_wait_runtime();
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
        info!(
            "Pause all requested: active_cues={}",
            self.cue_list
                .cues
                .iter()
                .filter(|cue| cue.is_active())
                .count()
        );
        let waiting_ids: HashSet<Uuid> = self
            .cue_list
            .cues
            .iter()
            .filter(|cue| cue.state == CueState::Waiting)
            .map(|cue| cue.id)
            .collect();
        let active_ids: HashSet<Uuid> = self
            .cue_list
            .cues
            .iter()
            .filter(|cue| cue.is_active())
            .map(|cue| cue.id)
            .collect();
        self.pending_triggers.retain(|trigger| {
            let waiting_target = waiting_ids.contains(&trigger.cue_id)
                || trigger
                    .source_cue_id
                    .is_some_and(|source_id| waiting_ids.contains(&source_id));
            let active_auto_chain = matches!(
                trigger.action,
                PendingAction::TriggerCue | PendingAction::BeginPostWait | PendingAction::ClearWait
            ) && trigger
                .source_cue_id
                .is_some_and(|source_id| active_ids.contains(&source_id));
            !waiting_target && !active_auto_chain
        });
        for cue in &mut self.cue_list.cues {
            if waiting_ids.contains(&cue.id) {
                cue.state = CueState::Stopped;
                cue.position_secs = 0.0;
            }
            cue.clear_wait_runtime();
        }
        self.cue_list.pause_all();
        self.audio.send(AudioCommand::PauseAll);
        self.status_msg = "⏸ All paused".to_string();
    }

    fn resume_selected(&mut self) {
        let Some(id) = self.selected else {
            return;
        };
        if self.is_comment_cue(id) {
            if let Some(next_id) = self.next_non_comment_after(id) {
                self.select_only(next_id);
            }
            return;
        }
        let Some(snapshot) = self.cue_list.find(id).cloned() else {
            return;
        };

        match snapshot.state {
            CueState::Paused => {
                info!(
                    "Resume selected: id={}, number='{}', name='{}', position={:.3}s",
                    snapshot.id, snapshot.number, snapshot.name, snapshot.position_secs
                );
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
        info!("Resume all requested");
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
            let default_route_channels = self
                .audio
                .default_route_channels_for_source(None, cue.audio_channels);
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
        cue.waveform = generate_waveform(&final_path, WAVEFORM_BUCKETS);

        let name = cue.name.clone();
        info!(
            "Audio file imported: path='{}', channels={}, duration={:.3}s",
            final_path.display(),
            cue.audio_channels,
            cue.duration_secs
        );
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
        self.pending_triggers
            .retain(|trigger| trigger.cue_id != id && trigger.source_cue_id != Some(id));
        self.bump_audio_generation(id);
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

    fn select_all_cues(&mut self) {
        self.selected_ids = self.cue_list.cues.iter().map(|cue| cue.id).collect();
        self.selected = visible_cue_ids(&self.cue_list.cues, &self.expanded_groups)
            .into_iter()
            .find(|id| self.selected_ids.contains(id));
        if self.selected.is_none() {
            self.selected = self.cue_list.cues.first().map(|cue| cue.id);
        }
        self.selection_anchor = self.selected;
        self.status_msg = format!("Selected {} events", self.selected_ids.len());
    }

    /// Move all selected rows as a block while retaining their relative order.
    /// Unselected rows between disjoint selections remain in their original order.
    fn move_selected_cues(&mut self, up: bool) {
        let selected_ids = if self.selected_ids.is_empty() {
            self.selected.into_iter().collect::<HashSet<_>>()
        } else {
            self.selected_ids.clone()
        };
        if selected_ids.is_empty() {
            return;
        }

        let cues = &mut self.cue_list.cues;
        if up {
            for index in 1..cues.len() {
                if selected_ids.contains(&cues[index].id)
                    && !selected_ids.contains(&cues[index - 1].id)
                {
                    cues.swap(index, index - 1);
                }
            }
        } else if cues.len() > 1 {
            for index in (0..cues.len() - 1).rev() {
                if selected_ids.contains(&cues[index].id)
                    && !selected_ids.contains(&cues[index + 1].id)
                {
                    cues.swap(index, index + 1);
                }
            }
        }
    }

    fn select_next(&mut self) {
        let visible = visible_cue_ids(&self.cue_list.cues, &self.expanded_groups);
        if visible.is_empty() {
            return;
        }
        let current_position = self
            .selected
            .and_then(|id| visible.iter().position(|item| *item == id));
        let start = current_position.map(|position| position + 1).unwrap_or(0);
        if let Some(next) = visible
            .iter()
            .skip(start)
            .copied()
            .find(|id| !self.is_comment_cue(*id))
        {
            self.select_only(next);
            self.scroll_to_cue = Some(next);
        }
    }

    fn select_prev(&mut self) {
        let visible = visible_cue_ids(&self.cue_list.cues, &self.expanded_groups);
        if visible.is_empty() {
            return;
        }
        let previous = if let Some(position) = self
            .selected
            .and_then(|id| visible.iter().position(|item| *item == id))
        {
            visible
                .iter()
                .take(position)
                .rev()
                .copied()
                .find(|id| !self.is_comment_cue(*id))
        } else {
            visible.iter().copied().find(|id| !self.is_comment_cue(*id))
        };
        if let Some(previous) = previous {
            self.select_only(previous);
            self.scroll_to_cue = Some(previous);
        }
    }

    // ── Сохранение и загрузка проекта ────────────────────────────────────────

    fn save_project(&mut self) {
        // Renaming an already-open project changes its in-memory identity only.
        // Ctrl+S must therefore create a new file instead of overwriting or
        // renaming the file that was originally opened.
        if self.project_needs_save_as {
            self.status_msg =
                "Project name changed — choose a new project file with Save As".to_string();
            self.save_as_project();
            return;
        }

        if self.project_path.is_none() {
            let default_name = format!("{}.tsq", self.project.settings.name);
            let path = FileDialog::new()
                .add_filter("Theatre Show Cue Project", &["tsq", "clabs"])
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
                info!(
                    "Project saved: path='{}', cues={}",
                    path.display(),
                    self.cue_list.cues.len()
                );
                self.status_msg = format!("💾 Saved: {}", path.display());
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
        let default_name = format!("{}.tsq", self.project.settings.name);

        let Some(path) = FileDialog::new()
            .add_filter("Theatre Show Cue Project", &["tsq", "clabs"])
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
                info!(
                    "Project saved as: path='{}', cues={}",
                    path.display(),
                    self.cue_list.cues.len()
                );
                self.project = project;
                self.project_path = Some(path.clone());
                self.assets.set_project_path(path.clone());
                self.report.project_name = self.project.settings.name.clone();
                self.project_needs_save_as = false;
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
    /// <parent>/<project>/<project>.tsq + audio/ with relative media paths.
    fn save_as_bundle(&mut self) {
        let default_name = self.project.settings.name.clone();
        let default_file_name = format!("{default_name}.tsq");

        let Some(selected_path) = FileDialog::new()
            .add_filter("Theatre Show Cue Project Bundle", &["tsq", "clabs"])
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
        let bundle_project_path = bundle_dir.join(format!("{bundle_name}.tsq"));
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
                info!(
                    "Project bundle saved: folder='{}', copied_audio={}, missing_audio={}, cues={}",
                    bundle_dir.display(),
                    copied_count,
                    missing_count,
                    bundle_cues.cues.len()
                );
                self.cue_list = bundle_cues;
                self.cue_clipboard.clear();
                self.project = project;
                self.project_path = Some(bundle_project_path.clone());
                self.assets.set_project_path(bundle_project_path);
                self.report.project_name = self.project.settings.name.clone();
                self.project_needs_save_as = false;
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
            self.last_backup_at = Instant::now();
            return;
        }

        self.project.cue_list = self.cue_list.clone();
        let safe_name = sanitize_project_name(&self.project.settings.name)
            .unwrap_or_else(|| "Project".to_string());
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S_%f");
        let backup_path = backup_dir.join(format!("{safe_name}__{timestamp}.tsq"));

        match self.project.save_backup(&backup_path, &project_path) {
            Ok(()) => {
                self.last_backup_at = Instant::now();
                info!("Created {reason} backup: {:?}", backup_path);

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
                                        extension.eq_ignore_ascii_case("tsq")
                                            || extension.eq_ignore_ascii_case("clabs")
                                    })
                        })
                        .collect(),
                    Err(error) => {
                        warn!("Could not list project backups: {error}");
                        Vec::new()
                    }
                };
                // Keep ten snapshots for this project, ordered by actual file
                // modification time rather than by project-name prefix.
                backups.retain(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(&format!("{safe_name}__")))
                });
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
                self.last_backup_at = Instant::now();
            }
        }
    }

    fn poll_periodic_backup(&mut self) {
        let interval_minutes = self.settings.backup_interval_minutes;
        if interval_minutes == 0
            || self.project_path.is_none()
            || self.last_backup_at.elapsed() < Duration::from_secs(interval_minutes as u64 * 60)
        {
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

            if cue.duration_secs > 0.0 && cue.waveform.len() >= WAVEFORM_BUCKETS.saturating_mul(2) {
                continue;
            }

            let duration = get_duration(&cue.file_path);
            if duration <= 0.0 {
                continue;
            }
            let waveform = generate_waveform(&cue.file_path, WAVEFORM_BUCKETS);
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
                    .default_route_channels_for_source(cue.io.output_device.as_deref(), channels);
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
        let waveform = generate_waveform(&source_path, WAVEFORM_BUCKETS);
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

        let Some(cue_snapshot) = self.cue_list.find(cue_id).cloned() else {
            return;
        };
        if !matches!(&cue_snapshot.cue_type, CueType::Audio) {
            return;
        }
        if cue_snapshot.is_active() {
            self.bump_audio_generation(cue_id);
            self.audio.send(AudioCommand::Stop(cue_id));
        }

        if let Some(cue) = self.cue_list.find_mut(cue_id) {
            cue.clear_wait_runtime();
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
                    .default_route_channels_for_source(cue.io.output_device.as_deref(), channels);
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

        info!(
            "Audio media replaced: cue_id={}, path='{}', channels={}, duration={:.3}s",
            cue_id,
            destination.display(),
            channels,
            duration
        );
        self.status_msg = format!(
            "Audio file updated: {} · {}",
            destination.display(),
            crate::ui::cue_table::format_duration(duration)
        );
    }

    fn open_project(&mut self) {
        self.stop_all();

        let file = FileDialog::new()
            .add_filter("Theatre Show Cue Project", &["tsq", "clabs"])
            .pick_file();

        if let Some(path) = file {
            match Project::load(&path) {
                Ok(mut proj) => {
                    let mut cl = proj.cue_list.clone();
                    if proj.version < 3 {
                        // Generic conversion of legacy native-index routing to
                        // logical TaSQ destinations. This occurs only once when
                        // loading an old project, never when changing Mapping.
                        for cue in &mut cl.cues {
                            if !cue_owns_audio_stream(cue) {
                                continue;
                            }
                            let backend = cue.io.output_backend.unwrap_or(self.audio.backend);
                            let device_name = cue
                                .io
                                .output_device
                                .as_deref()
                                .unwrap_or(self.audio.current_device.as_str());
                            let device_key = format!("{}::{}", backend.label(), device_name);
                            let output_count =
                                self.audio.output_channels_for_device(Some(device_name));
                            let order: Vec<usize> = self
                                .settings
                                .output_channel_orders
                                .get(&device_key)
                                .cloned()
                                .unwrap_or_else(|| (0..output_count).collect());
                            cue.io.upgrade_native_routes_to_logical(&order);
                        }
                        info!(
                            "Upgraded project routing schema {} to 3 (logical Cue outputs)",
                            proj.version
                        );
                        proj.version = 3;
                    }
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
                                let default_route_channels =
                                    self.audio.default_route_channels_for_source(
                                        cue.io.output_device.as_deref(),
                                        cue.audio_channels,
                                    );
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
                            if cue.waveform.len() < WAVEFORM_BUCKETS * 2 {
                                cue.waveform = generate_waveform(&cue.file_path, WAVEFORM_BUCKETS);
                            }
                        }
                    }

                    proj.cue_list = cl.clone();
                    self.cue_list = cl;
                    self.cue_clipboard.clear();
                    self.time_trigger_last_fired.clear();
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
                    self.last_backup_at = Instant::now();
                    self.last_media_scan_at = Instant::now();
                    self.report = SessionReport::new(&self.project.settings.name);
                    self.selected = None;
                    self.selected_ids.clear();
                    self.selection_anchor = None;
                    info!(
                        "Project opened: path='{}', cues={}",
                        path.display(),
                        self.cue_list.cues.len()
                    );
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
        info!("New project requested");
        self.stop_all();
        self.cue_list = CueList::new("Main");
        self.cue_clipboard.clear();
        self.time_trigger_last_fired.clear();
        self.project = Project::default();
        self.project_path = None;
        self.project_needs_save_as = false;
        self.assets = AssetManager::new();
        self.report = SessionReport::new("Untitled Project");
        self.selected = None;
        self.selected_ids.clear();
        self.selection_anchor = None;
        self.last_backup_at = Instant::now();
        self.last_media_scan_at = Instant::now();
        self.status_msg = "New project".to_string();
    }

    // ── Опрос аудио, MIDI и OSC ──────────────────────────────────────────────

    fn poll_audio(&mut self) {
        let mut finished_cues = Vec::new();

        for update in self.audio.drain_position_updates() {
            if self.audio_generations.get(&update.id).copied() != Some(update.generation) {
                continue;
            }
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
        let mut completed_fade_clocks = Vec::new();
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
                    if elapsed >= duration {
                        if fade_cue.post_wait_secs > 0.0 {
                            fade_cue.state = CueState::Waiting;
                        }
                        completed_fade_clocks.push(fade_id);
                    }
                } else {
                    completed_fade_clocks.push(fade_id);
                }
            } else {
                completed_fade_clocks.push(fade_id);
            }
        }
        for fade_id in completed_fade_clocks {
            self.fade_started_at.remove(&fade_id);
        }
    }

    fn poll_wait_progress(&mut self) {
        let now = Instant::now();
        for cue in &mut self.cue_list.cues {
            let Some(started_at) = cue.wait_started_at else {
                continue;
            };
            if now < started_at {
                continue;
            }
            let elapsed = now.duration_since(started_at).as_secs_f64();
            if cue.wait_phase != WaitPhase::None
                && !matches!(
                    &cue.state,
                    CueState::Playing | CueState::FadingIn | CueState::FadingOut | CueState::Paused
                )
            {
                cue.state = CueState::Waiting;
                cue.position_secs = elapsed.min(cue.wait_duration_secs.max(0.0));
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
                            !matches!(&c.cue_type, CueType::Comment { .. })
                                && matches!(
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

    fn copy_selected_cues(&mut self) {
        let mut include = if self.selected_ids.is_empty() {
            self.selected.into_iter().collect::<HashSet<_>>()
        } else {
            self.selected_ids.clone()
        };
        if include.is_empty() {
            self.status_msg = "Select one or more events to copy".to_string();
            return;
        }

        // Copy a selected Group together with its descendants, preserving the
        // internal hierarchy. Multi-selection is kept in Cue List order.
        let mut stack: Vec<Uuid> = include.iter().copied().collect();
        while let Some(id) = stack.pop() {
            if let Some(cue) = self.cue_list.find(id) {
                if let CueType::Group { children, .. } = &cue.cue_type {
                    for child_id in children {
                        if include.insert(*child_id) {
                            stack.push(*child_id);
                        }
                    }
                }
            }
        }

        let mut copied: Vec<Cue> = self
            .cue_list
            .cues
            .iter()
            .filter(|cue| include.contains(&cue.id))
            .cloned()
            .collect();
        for cue in &mut copied {
            cue.state = CueState::Stopped;
            cue.position_secs = 0.0;
            cue.clear_wait_runtime();
        }
        self.cue_clipboard = copied;
        self.status_msg = format!("Copied {} event(s)", self.cue_clipboard.len());
    }

    fn paste_cues_after(&mut self, anchor_id: Option<Uuid>) {
        if self.cue_clipboard.is_empty() {
            self.status_msg = "Clipboard is empty".to_string();
            return;
        }

        let mut pasted = self.cue_clipboard.clone();
        let id_map: HashMap<Uuid, Uuid> =
            pasted.iter().map(|cue| (cue.id, Uuid::new_v4())).collect();

        for cue in &mut pasted {
            let old_id = cue.id;
            cue.id = id_map[&old_id];
            cue.number.clear();
            cue.state = CueState::Stopped;
            cue.position_secs = 0.0;
            cue.clear_wait_runtime();

            let remap_target = |target: &mut Option<Uuid>| {
                if let Some(old_target) = *target {
                    if let Some(new_target) = id_map.get(&old_target) {
                        *target = Some(*new_target);
                    }
                }
            };
            match &mut cue.cue_type {
                CueType::Play { target_id }
                | CueType::Pause { target_id }
                | CueType::Stop { target_id }
                | CueType::Fade { target_id, .. } => remap_target(target_id),
                CueType::Group { children, .. } => {
                    for child_id in children {
                        if let Some(new_child_id) = id_map.get(child_id) {
                            *child_id = *new_child_id;
                        }
                    }
                }
                _ => {}
            }
        }

        // A copied group includes its children. Only top-level copied cues are
        // inserted into the destination group's child list.
        let copied_child_ids: HashSet<Uuid> = pasted
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
        let root_ids: Vec<Uuid> = pasted
            .iter()
            .filter(|cue| !copied_child_ids.contains(&cue.id))
            .map(|cue| cue.id)
            .collect();

        let insert_at = anchor_id
            .and_then(|anchor| {
                self.cue_list
                    .cues
                    .iter()
                    .position(|cue| cue.id == anchor)
                    .map(|index| index + 1)
            })
            .unwrap_or(self.cue_list.cues.len());

        let parent_id = anchor_id.and_then(|anchor| self.parent_group_of(anchor));
        if let (Some(parent_id), Some(anchor)) = (parent_id, anchor_id) {
            if let Some(parent) = self.cue_list.find_mut(parent_id) {
                if let CueType::Group { children, .. } = &mut parent.cue_type {
                    let child_insert_at = children
                        .iter()
                        .position(|child_id| *child_id == anchor)
                        .map(|index| index + 1)
                        .unwrap_or(children.len());
                    for (offset, new_id) in root_ids.iter().copied().enumerate() {
                        children.insert(child_insert_at + offset, new_id);
                    }
                }
            }
        }

        let mut next_number = self
            .cue_list
            .cues
            .iter()
            .filter_map(|cue| cue.number.trim().parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        for cue in &mut pasted {
            cue.number = next_number.to_string();
            next_number = next_number.saturating_add(1);
        }

        let pasted_count = pasted.len();
        for (offset, cue) in pasted.into_iter().enumerate() {
            let insert_position = (insert_at + offset).min(self.cue_list.cues.len());
            self.cue_list.cues.insert(insert_position, cue);
        }

        self.selected_ids = root_ids.iter().copied().collect();
        self.selected = root_ids.first().copied();
        self.selection_anchor = self.selected;
        self.status_msg = format!("Pasted {} event(s)", pasted_count);
    }

    // ── Горячие клавиши и Drag-and-drop ──────────────────────────────────────

    fn handle_keyboard(&mut self, ctx: &Context) {
        // The settings window is modal. Keep its explicit hotkey capture active,
        // but never dispatch normal transport or project shortcuts behind it.
        if self.settings_open && self.hotkey_capture.is_none() {
            return;
        }

        // egui reports logical keys for the active keyboard layout and falls
        // back to the physical key when a non-Latin layout cannot be mapped.
        let key_events: Vec<(Key, Option<Key>, egui::Modifiers)> = ctx.input(|input| {
            input
                .events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::Key {
                        key,
                        physical_key,
                        pressed: true,
                        modifiers,
                        ..
                    } => Some((*key, *physical_key, *modifiers)),
                    _ => None,
                })
                .collect()
        });

        // Capture mode has priority over normal application shortcuts.
        if let Some(action) = self.hotkey_capture.clone() {
            if let Some((key, _, modifiers)) = key_events.first().copied() {
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

        // Do not steal keystrokes from text fields and other editors.
        if ctx.wants_keyboard_input() {
            return;
        }

        // Check both the layout-dependent logical key and the physical key.
        // The latter keeps shortcuts stable when the user switches to Russian.
        let has_ctrl_shortcut = |target: Key, shift: bool, alt: bool| {
            key_events.iter().any(|(logical, physical, modifiers)| {
                (modifiers.ctrl || modifiers.command)
                    && modifiers.shift == shift
                    && modifiers.alt == alt
                    && (*logical == target || *physical == Some(target))
            })
        };

        if has_ctrl_shortcut(Key::S, true, false) {
            self.save_as_project();
            return;
        }
        if has_ctrl_shortcut(Key::S, false, true) {
            self.save_as_bundle();
            return;
        }
        if has_ctrl_shortcut(Key::C, false, false) {
            self.copy_selected_cues();
            if !self.cue_clipboard.is_empty() {
                ctx.copy_text(INTERNAL_CUE_CLIPBOARD_MARKER.to_string());
            }
            return;
        }
        if has_ctrl_shortcut(Key::V, false, false) {
            self.paste_cues_after(self.selected);
            return;
        }
        if has_ctrl_shortcut(Key::A, false, false) {
            self.select_all_cues();
            return;
        }

        // Some egui-winit versions emit clipboard events instead of a Key event.
        // Only the private marker represents Theatre Show Cue's internal cue buffer; arbitrary
        // clipboard text is never interpreted as a cue paste command.
        let clipboard_payload = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Paste(text) => Some(text.clone()),
                _ => None,
            })
        });
        if clipboard_payload.as_deref() == Some(INTERNAL_CUE_CLIPBOARD_MARKER)
            && !self.cue_clipboard.is_empty()
        {
            self.paste_cues_after(self.selected);
            return;
        }
        let clipboard_copy = ctx.input(|input| {
            input
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::Copy))
        });
        if clipboard_copy {
            self.copy_selected_cues();
            if !self.cue_clipboard.is_empty() {
                ctx.copy_text(INTERNAL_CUE_CLIPBOARD_MARKER.to_string());
            }
            return;
        }

        if let Some(selected_id) = self.selected {
            let selected_is_group = self
                .cue_list
                .find(selected_id)
                .map(|cue| matches!(&cue.cue_type, CueType::Group { .. }))
                .unwrap_or(false);
            if selected_is_group
                && key_events.iter().any(|(key, physical, _)| {
                    *key == Key::ArrowRight || *physical == Some(Key::ArrowRight)
                })
            {
                self.expanded_groups.insert(selected_id);
                return;
            }
            if key_events.iter().any(|(key, physical, _)| {
                *key == Key::ArrowLeft || *physical == Some(Key::ArrowLeft)
            }) {
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
            let Some(binding) = self.settings.hotkeys.bindings.get(action) else {
                continue;
            };
            if key_events.iter().any(|(key, physical, modifiers)| {
                Self::format_hotkey(*key, *modifiers).eq_ignore_ascii_case(binding)
                    || physical
                        .as_ref()
                        .map(|physical_key| {
                            Self::format_hotkey(*physical_key, *modifiers)
                                .eq_ignore_ascii_case(binding)
                        })
                        .unwrap_or(false)
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
        info!("Application command dispatched: {}", action.label());
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
                let selected_ids = if self.selected_ids.is_empty() {
                    self.selected.into_iter().collect::<HashSet<_>>()
                } else {
                    self.selected_ids.clone()
                };
                let ids: Vec<Uuid> = self
                    .cue_list
                    .cues
                    .iter()
                    .filter(|cue| selected_ids.contains(&cue.id))
                    .map(|cue| cue.id)
                    .collect();
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
            AppAction::MoveUp => self.move_selected_cues(true),
            AppAction::MoveDown => self.move_selected_cues(false),
        }
    }

    fn switch_audio_endpoint(
        &mut self,
        backend: AudioBackend,
        device_name: &str,
        endpoint_id: &str,
    ) -> bool {
        let same_endpoint =
            self.audio.backend == backend && self.audio.current_device_id == endpoint_id;
        if same_endpoint && !self.active_output_needs_reopen {
            return true;
        }

        let previous_backend = self.audio.backend;
        let backend_changed = previous_backend != backend;
        let reinitialize_current = same_endpoint && self.active_output_needs_reopen;
        let performance = AudioPerformanceSettings {
            sample_rate: self.settings.audio_sample_rate,
            buffer_size_frames: self.settings.audio_buffer_size_frames,
        };
        info!(
            "Audio endpoint switch requested: {} — '{}' -> {} — '{}'",
            previous_backend.short_label(),
            self.audio.current_device,
            backend.short_label(),
            device_name
        );

        if backend_changed {
            self.stop_all();
            let previous_device_registry = self.audio.device_registry.clone();
            let mut replacement: AudioEngine =
                match AudioEngine::new_with_backend_and_endpoint_using_registry_and_performance(
                    backend,
                    Some(endpoint_id),
                    Some(device_name),
                    previous_device_registry.clone(),
                    performance,
                ) {
                    Ok(engine) => engine,
                    Err(error)
                        if backend_changed
                            && performance != AudioPerformanceSettings::default() =>
                    {
                        warn!(
                            "The selected endpoint does not accept the previous performance profile {:?}: {}. Retrying with Automatic settings.",
                            performance,
                            error
                        );
                        match AudioEngine::new_with_backend_and_endpoint_using_registry_and_performance(
                            backend,
                            Some(endpoint_id),
                            Some(device_name),
                            previous_device_registry,
                            AudioPerformanceSettings::default(),
                        ) {
                            Ok(engine) => {
                                self.settings.audio_sample_rate = None;
                                self.settings.audio_buffer_size_frames = None;
                                engine
                            }
                            Err(fallback_error) => {
                                self.status_msg = format!(
                                    "Could not open {} — {} with saved or Automatic audio settings: {}",
                                    backend.short_label(),
                                    device_name,
                                    fallback_error
                                );
                                warn!("{}", self.status_msg);
                                self.active_output_needs_reopen = false;
                                return false;
                            }
                        }
                    }
                    Err(error) => {
                        self.status_msg = format!(
                            "Could not open {} — {}: {}",
                            backend.short_label(),
                            device_name,
                            error
                        );
                        warn!("{}", self.status_msg);
                        self.active_output_needs_reopen = false;
                        return false;
                    }
                };

            replacement.set_output_channel_names(self.settings.output_channel_names.clone());
            replacement.set_default_output_pairs(self.settings.default_output_pairs.clone());
            replacement.set_output_channel_orders(self.settings.output_channel_orders.clone());

            let backend_devices = replacement.output_devices.clone();
            for device in &backend_devices {
                let settings_key = replacement.output_device_settings_key(device);
                if let Some(device_settings) = self
                    .settings
                    .output_channel_settings_by_device
                    .get(&settings_key)
                {
                    replacement
                        .set_output_channel_settings_for_device(device, device_settings.clone());
                }
            }

            // Keep the selected endpoint's saved calibration, falling back to the
            // previous global channel settings only when no per-device profile exists.
            let selected_device_name = replacement.current_device.clone();
            let selected_key = replacement.output_device_settings_key(&selected_device_name);
            if let Some(device_settings) = self
                .settings
                .output_channel_settings_by_device
                .get(&selected_key)
            {
                replacement.set_output_channel_settings(device_settings.clone());
            } else {
                replacement.set_output_channel_settings(self.settings.output_channels.clone());
            }

            self.audio = replacement;

            if backend_changed {
                // Old project files stored only a device name. Pin those legacy
                // explicit routes to the backend that was active before this switch.
                for cue in &mut self.cue_list.cues {
                    if cue.io.output_device.is_some() && cue.io.output_backend.is_none() {
                        cue.io.output_backend = Some(previous_backend);
                    }
                }
            }
        } else if reinitialize_current {
            self.stop_all();
            if let Err(error) = self.audio.reconfigure_output(performance) {
                self.status_msg = format!(
                    "Could not apply audio performance settings to {} — {}: {}",
                    backend.short_label(),
                    device_name,
                    error
                );
                warn!("{}", self.status_msg);
                self.active_output_needs_reopen = false;
                return false;
            }
        } else if let Err(error) =
            self.audio
                .set_output_endpoint(endpoint_id, device_name, performance)
        {
            if !same_endpoint && performance != AudioPerformanceSettings::default() {
                warn!(
                    "The selected {} endpoint '{}' rejected performance settings {:?}: {}. Retrying with Automatic settings.",
                    backend.short_label(),
                    device_name,
                    performance,
                    error
                );
                if let Err(fallback_error) = self.audio.set_output_endpoint(
                    endpoint_id,
                    device_name,
                    AudioPerformanceSettings::default(),
                ) {
                    self.status_msg = format!(
                        "Could not switch to {} — {} with saved or Automatic audio settings: {}",
                        backend.short_label(),
                        device_name,
                        fallback_error
                    );
                    warn!("{}", self.status_msg);
                    self.active_output_needs_reopen = false;
                    return false;
                }
                self.settings.audio_sample_rate = None;
                self.settings.audio_buffer_size_frames = None;
            } else {
                self.status_msg = format!(
                    "Could not switch to {} — {}: {}",
                    backend.short_label(),
                    device_name,
                    error
                );
                warn!("{}", self.status_msg);
                self.active_output_needs_reopen = false;
                return false;
            }
        }

        self.active_output_needs_reopen = false;
        let active_device_name = self.audio.current_device.clone();
        for cue in &mut self.cue_list.cues {
            let cue_backend = cue
                .io
                .output_backend
                .or_else(|| {
                    cue.io
                        .output_device_id
                        .as_deref()
                        .and_then(|id| self.audio.device_registry.find_by_id(id))
                        .map(|endpoint| endpoint.backend)
                })
                .unwrap_or(backend);
            if let Some(endpoint_id) = cue.io.output_device_id.as_deref() {
                if let Some(endpoint) = self.audio.device_registry.find_available_by_id(endpoint_id)
                {
                    if endpoint.backend == cue_backend {
                        cue.io.output_device = Some(endpoint.device_name.clone());
                        cue.io.output_backend = Some(endpoint.backend);
                    }
                }
            } else if let Some(device_name) = cue.io.output_device.clone() {
                if let Some(endpoint_id) =
                    self.audio.registered_endpoint_id(cue_backend, &device_name)
                {
                    cue.io.output_device_id = Some(endpoint_id);
                    cue.io.output_backend = Some(cue_backend);
                }
            }
        }

        self.settings.audio_backend = backend;
        self.settings.audio_output_device = Some(active_device_name.clone());
        self.settings.audio_output_device_id = Some(self.audio.current_device_id.clone());
        self.settings.output_channels = self.audio.output_channel_settings();

        let selected_key = self.audio.output_device_settings_key(&active_device_name);
        self.settings
            .output_channel_settings_by_device
            .insert(selected_key, self.settings.output_channels.clone());

        // Rebuild defaults only for cues that use the active backend or the
        // global default. Routes pinned to another backend are preserved for Stage 4.
        for cue in &mut self.cue_list.cues {
            if !matches!(&cue.cue_type, CueType::Audio) || cue.audio_channels == 0 {
                continue;
            }
            let cue_backend = cue.io.output_backend.unwrap_or(backend);
            if cue.io.output_device.is_some() && cue_backend != backend {
                continue;
            }
            let output_count = self
                .audio
                .output_channels_for_device(cue.io.output_device.as_deref());
            let default_route_channels = self.audio.default_route_channels_for_source(
                cue.io.output_device.as_deref(),
                cue.audio_channels,
            );
            cue.io.ensure_channel_routes_with_defaults(
                cue.audio_channels,
                output_count,
                &default_route_channels,
            );
        }

        self.save_app_settings();
        if let Err(error) = crate::diagnostics::write_runtime_report(
            self.audio.backend.label(),
            &self.audio.output_devices,
            &self.audio.input_devices,
            self.audio.output_channels,
        ) {
            warn!("Could not refresh diagnostic report after endpoint switch: {error}");
        }

        info!(
            "Audio endpoint switch completed: backend='{}', output='{}', channels={}",
            self.audio.backend.label(),
            self.audio.current_device,
            self.audio.output_channels
        );
        self.status_msg = format!(
            "Audio output selected: {} — {}",
            backend.short_label(),
            active_device_name
        );
        true
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

        egui::Window::new("Theatre Show Cue Settings")
            .open(&mut is_open)
            .default_size([820.0, 580.0])
            // No forced minimum or maximum: if content does not fit, scroll it.
            .resizable(true)
            .order(egui::Order::Foreground)
            .frame(egui::Frame::window(&ctx.style()).fill(Theme::bg_panel()))
            .show(ctx, |ui| {
                egui::ScrollArea::horizontal()
                    .id_salt("settings_tab_strip")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for (tab, label) in [
                                (SettingsTab::Audio, "Audio"),
                                (SettingsTab::Theme, "Theme"),
                                (SettingsTab::Osc, "OSC"),
                                (SettingsTab::Hotkeys, "Hotkeys"),
                                (SettingsTab::MidiControl, "MIDI Control"),
                                (SettingsTab::Midi, "MIDI Interfaces"),
                                (SettingsTab::Playback, "Playback"),
                                (SettingsTab::BackupLogs, "Backup & Logs"),
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
                    SettingsTab::Theme => {
                        ui.heading("Theme");
                        ui.label(
                            RichText::new("Choose a contrast preset or customize the window and text colors.")
                                .color(Theme::text_dim())
                                .size(11.0),
                        );
                        ui.add_space(8.0);

                        let mut appearance_changed = false;
                        ui.horizontal_wrapped(|ui| {
                            ui.label("Theme preset:");
                            for preset in ThemePreset::ALL {
                                if ui
                                    .selectable_value(
                                        &mut self.settings.appearance.preset,
                                        *preset,
                                        preset.label(),
                                    )
                                    .changed()
                                {
                                    appearance_changed = true;
                                }
                            }
                        });
                        ui.horizontal_wrapped(|ui| {
                            ui.label("Visual design:");
                            for design in UiDesign::ALL {
                                if ui
                                    .selectable_value(
                                        &mut self.settings.appearance.design,
                                        *design,
                                        design.label(),
                                    )
                                    .changed()
                                {
                                    appearance_changed = true;
                                }
                            }
                        });
                        ui.label(
                            RichText::new(
                                "Liquid Glass uses translucent, rounded surfaces; Material uses flatter, crisp controls. True backdrop blur is not available in the current egui renderer.",
                            )
                            .color(Theme::text_dim())
                            .size(10.5),
                        );

                        if self.settings.appearance.preset == ThemePreset::Custom {
                            ui.separator();
                            ui.label("Custom colors");
                            appearance_changed |= edit_rgb_color(
                                ui,
                                "Main window background",
                                &mut self.settings.appearance.window_color,
                            );
                            appearance_changed |= edit_rgb_color(
                                ui,
                                "Panel / inspector background",
                                &mut self.settings.appearance.panel_color,
                            );
                            appearance_changed |= edit_rgb_color(
                                ui,
                                "Primary text and menu labels",
                                &mut self.settings.appearance.text_color,
                            );
                            appearance_changed |= edit_rgb_color(
                                ui,
                                "Secondary / informational text",
                                &mut self.settings.appearance.muted_text_color,
                            );
                            appearance_changed |= edit_rgb_color(
                                ui,
                                "Loop region color",
                                &mut self.settings.appearance.loop_region_color,
                            );
                            appearance_changed |= edit_rgb_color(
                                ui,
                                "Toolbar and standard button color",
                                &mut self.settings.appearance.button_color,
                            );
                            appearance_changed |= edit_rgb_color(
                                ui,
                                "Group outline color",
                                &mut self.settings.appearance.group_outline_color,
                            );
                            appearance_changed |= edit_rgb_color(
                                ui,
                                "Input field background",
                                &mut self.settings.appearance.input_color,
                            );
                            appearance_changed |= edit_rgb_color(
                                ui,
                                "GO button",
                                &mut self.settings.appearance.go_button_color,
                            );
                            appearance_changed |= edit_rgb_color(
                                ui,
                                "STOP button",
                                &mut self.settings.appearance.stop_button_color,
                            );
                            appearance_changed |= edit_rgb_color(
                                ui,
                                "PAUSE button",
                                &mut self.settings.appearance.pause_button_color,
                            );
                            ui.label(
                                RichText::new("Custom colors include toolbar buttons, group outlines, text inputs, transport controls, panels, text and the loop region.")
                                    .color(Theme::text_dim())
                                    .size(11.0),
                            );
                        } else {
                            ui.add_space(4.0);
                            let description = match self.settings.appearance.preset {
                                ThemePreset::Dark => "Balanced dark interface with soft gray text.",
                                ThemePreset::Light => "Light interface with dark, high-contrast text.",
                                ThemePreset::Black => "Near-black interface with bright text for dark rooms and OLED displays.",
                                ThemePreset::Custom => "Custom color palette.",
                            };
                            ui.label(RichText::new(description).color(Theme::text_dim()).size(11.0));
                        }

                        if appearance_changed {
                            Theme::apply(ctx, &self.settings.appearance);
                            self.save_app_settings();
                        }
                    }
                    SettingsTab::Audio => {
                        ui.heading("Global audio interface");
                        ui.label(
                            RichText::new(
                                "Choose the output endpoint together with its driver. The driver is selected automatically from the device entry.",
                            )
                            .color(Theme::text_dim())
                            .size(11.0),
                        );

                        let selected_endpoint_label = self
                            .audio
                            .device_registry
                            .find_by_id(&self.audio.current_device_id)
                            .map(|endpoint| endpoint.label())
                            .unwrap_or_else(|| {
                                format!(
                                    "{} — {} (not in registry)",
                                    self.audio.backend.short_label(),
                                    self.audio.current_device
                                )
                            });
                        let mut selected_endpoint = None;
                        ui.horizontal(|ui| {
                            ui.label("Output device");
                            egui::ComboBox::from_id_salt("settings_output_endpoint")
                                .width(390.0)
                                .selected_text(&selected_endpoint_label)
                                .show_ui(ui, |ui| {
                                    for endpoint in self.audio.device_registry.endpoints().to_vec() {
                                        let selected = endpoint.id == self.audio.current_device_id;
                                        let label = endpoint.label();
                                        ui.add_enabled_ui(endpoint.is_available(), |ui| {
                                            if ui.selectable_label(selected, label).clicked() {
                                                selected_endpoint = Some(endpoint.clone());
                                            }
                                        });
                                    }
                                });
                            if ui.button("Refresh devices").clicked() {
                                self.device_registry_monitor.request_refresh();
                                self.status_msg = "WASAPI/DirectSound device scan requested".to_string();
                            }
                        });

                        let last_scan = self
                            .audio
                            .device_registry
                            .last_scan_elapsed()
                            .map(|elapsed| format!("{}s ago", elapsed.as_secs()))
                            .unwrap_or_else(|| "not scanned yet".to_string());
                        ui.label(
                            RichText::new(format!(
                                "Device Registry: {} available · {} unavailable · generation {} · last scan {}",
                                self.audio.device_registry.available_count(),
                                self.audio.device_registry.unavailable_count(),
                                self.audio.device_registry.generation(),
                                last_scan
                            ))
                            .color(Theme::text_dim())
                            .size(10.5),
                        );
                        for error in self.audio.device_registry.last_errors() {
                            ui.colored_label(egui::Color32::YELLOW, error);
                        }
                        ui.label(
                            RichText::new(
                                "WASAPI/DirectSound are checked every 5 seconds. The ASIO driver inventory is loaded at startup and is not re-enumerated while the app is running, to avoid reinitializing vendor drivers. Restart the app after installing/removing ASIO drivers.",
                            )
                            .color(Theme::text_dim())
                            .size(10.0),
                        );

                        if let Some(endpoint) = selected_endpoint {
                            self.switch_audio_endpoint(
                                endpoint.backend,
                                &endpoint.device_name,
                                &endpoint.id,
                            );
                        }

                        let output = self.audio.current_device.clone();
                        ui.label(format!(
                            "Active stream: {} channels @ {} Hz",
                            self.audio.output_channels,
                            self.audio.current_sample_rate
                        ));

                        ui.separator();
                        ui.heading("Audio performance");
                        ui.label(
                            RichText::new(
                                "Sample rate and the backend-specific buffer target are applied by reopening the active output. Smaller targets can reduce queued latency but may cause dropouts; the actual callback period depends on the backend.",
                            )
                            .color(Theme::text_dim())
                            .size(10.5),
                        );

                        let previous_sample_rate = self.settings.audio_sample_rate;
                        let previous_buffer_size = self.settings.audio_buffer_size_frames;
                        let mut selected_sample_rate = previous_sample_rate;
                        let mut selected_buffer_size = previous_buffer_size;
                        let mut sample_rates = self.audio.supported_sample_rates.clone();
                        if let Some(rate) = selected_sample_rate {
                            if !sample_rates.contains(&rate) {
                                sample_rates.push(rate);
                                sample_rates.sort_unstable();
                            }
                        }
                        let mut buffer_sizes = self.audio.supported_buffer_sizes.clone();
                        // DirectSound has a fixed set of TaSQ queue targets;
                        // ASIO values must be supported by the driver. WASAPI
                        // Shared accepts app-requested ring-buffer capacities.
                        selected_buffer_size = normalize_buffer_selection(
                            self.audio.backend,
                            selected_buffer_size,
                            &buffer_sizes,
                        );
                        if let Some(frames) = selected_buffer_size {
                            if !buffer_sizes.contains(&frames) {
                                buffer_sizes.push(frames);
                                buffer_sizes.sort_unstable();
                            }
                        }

                        ui.horizontal(|ui| {
                            ui.label("Sample rate");
                            let rate_label = selected_sample_rate
                                .map(|rate| format!("{} Hz", rate))
                                .unwrap_or_else(|| {
                                    format!("Auto ({} Hz)", self.audio.current_sample_rate)
                                });
                            egui::ComboBox::from_id_salt("global_audio_sample_rate")
                                .width(190.0)
                                .selected_text(rate_label)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut selected_sample_rate,
                                        None,
                                        format!("Auto ({} Hz)", self.audio.current_sample_rate),
                                    );
                                    for rate in &sample_rates {
                                        ui.selectable_value(
                                            &mut selected_sample_rate,
                                            Some(*rate),
                                            format!("{} Hz", rate),
                                        );
                                    }
                                });
                        });

                        ui.horizontal(|ui| {
                            ui.label("Buffer size");
                            let default_buffer_label = match self.audio.backend {
                                AudioBackend::DirectSound => {
                                    let ms = 4_096.0 * 1_000.0
                                        / self.audio.current_sample_rate.max(1) as f64;
                                    format!("Auto (4096 frames / {:.1} ms)", ms)
                                }
                                AudioBackend::Wasapi => {
                                    "Automatic (Windows shared-mode default)".to_string()
                                }
                                AudioBackend::Asio => {
                                    "Automatic (ASIO driver default)".to_string()
                                }
                            };
                            let buffer_label = selected_buffer_size
                                .map(|frames| {
                                    let rate = selected_sample_rate.unwrap_or(self.audio.current_sample_rate).max(1);
                                    let ms = frames as f64 * 1_000.0 / rate as f64;
                                    format!("{} frames / {:.1} ms", frames, ms)
                                })
                                .unwrap_or_else(|| default_buffer_label.clone());
                            egui::ComboBox::from_id_salt("global_audio_buffer_size")
                                .width(240.0)
                                .selected_text(buffer_label)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut selected_buffer_size,
                                        None,
                                        default_buffer_label,
                                    );
                                    for frames in &buffer_sizes {
                                        let rate = selected_sample_rate.unwrap_or(self.audio.current_sample_rate).max(1);
                                        let ms = *frames as f64 * 1_000.0 / rate as f64;
                                        ui.selectable_value(
                                            &mut selected_buffer_size,
                                            Some(*frames),
                                            format!("{} frames / {:.1} ms", frames, ms),
                                        );
                                    }
                                });
                        });

                        ui.label(
                            RichText::new(match self.audio.backend {
                                AudioBackend::Asio => "ASIO: the driver owns the hardware block size. TaSQ shows only sizes reported by the driver; use its control panel for other values.",
                                AudioBackend::Wasapi => "WASAPI Shared: this requests the shared ring-buffer capacity. Windows still controls the callback period, so actual timing and latency may differ.",
                                AudioBackend::DirectSound => "DirectSound: this controls TaSQ's queued-audio safety window. Larger queues improve stability but increase latency.",
                            })
                            .color(Theme::text_dim())
                            .size(10.0),
                        );

                        if selected_sample_rate != previous_sample_rate
                            || selected_buffer_size != previous_buffer_size
                        {
                            self.settings.audio_sample_rate = selected_sample_rate;
                            self.settings.audio_buffer_size_frames = selected_buffer_size;
                            let backend = self.audio.backend;
                            let device_name = self.audio.current_device.clone();
                            let endpoint_id = self.audio.current_device_id.clone();
                            self.active_output_needs_reopen = true;
                            if self.switch_audio_endpoint(backend, &device_name, &endpoint_id) {
                                self.save_app_settings();
                            } else {
                                self.settings.audio_sample_rate = previous_sample_rate;
                                self.settings.audio_buffer_size_frames = previous_buffer_size;
                                self.save_app_settings();
                            }
                        }

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
                        let mut output_settings = self.audio.output_channel_settings_for_device(Some(&output));
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

                        // Logical Theatre Show Cue output slots map to native driver
                        // channels only at playback; stored Cue routes are never rewritten.
                        let mut output_order = self.audio.output_channel_display_order(Some(&output));
                        let previous_output_order = output_order.clone();
                        let mut output_order_changed = false;
                        ui.collapsing("Logical output mapping", |ui| {
                            ui.label(
                                RichText::new(
                                    "Map TaSQ Output 1, 2, 3... to native driver channels. Test and Cue playback use this same map. Existing Cue routing and Default Stereo Pair never change when this map changes.",
                                )
                                .color(Theme::text_dim())
                                .size(11.0),
                            );
                            for logical_index in 0..output_count {
                                let selected_native = output_order
                                    .get(logical_index)
                                    .copied()
                                    .unwrap_or(logical_index);
                                ui.horizontal(|ui| {
                                    ui.add_sized(
                                        [150.0, 28.0],
                                        egui::Label::new(format!("TaSQ Output {}", logical_index + 1)),
                                    );
                                    let mut next_native = selected_native;
                                    egui::ComboBox::from_id_salt((
                                        "logical_output_channel_map",
                                        output.clone(),
                                        logical_index,
                                    ))
                                    .width(220.0)
                                    .selected_text(self.audio.output_channel_label(
                                        Some(&output),
                                        selected_native,
                                    ))
                                    .show_ui(ui, |ui| {
                                        // Show every native channel. Choosing one already
                                        // assigned to another logical slot swaps the two
                                        // assignments, so a full permutation remains editable.
                                        for native_index in 0..output_count {
                                            let label = self.audio.output_channel_label(
                                                Some(&output),
                                                native_index,
                                            );
                                            ui.selectable_value(
                                                &mut next_native,
                                                native_index,
                                                label,
                                            );
                                        }
                                    });
                                    if next_native != selected_native {
                                        let previous_native = selected_native;
                                        if let Some(other_logical) = output_order
                                            .iter()
                                            .position(|mapped| *mapped == next_native)
                                        {
                                            if other_logical != logical_index {
                                                output_order[other_logical] = previous_native;
                                            }
                                        }
                                        output_order[logical_index] = next_native;
                                        output_order_changed = true;
                                    }
                                });
                            }
                            if ui.small_button("Reset to native driver order").clicked() {
                                output_order = (0..output_count).collect();
                                output_order_changed = true;
                            }
                        });
                        if output_order_changed && output_order != previous_output_order {
                            // Only the global logical-to-native map changes. Neither
                            // existing Cue I/O Routing nor Default Stereo Pair changes.
                            self.audio.set_output_channel_order_for_device(
                                &output,
                                output_order.clone(),
                            );
                            let settings_key = self.audio.output_device_settings_key(&output);
                            self.settings.output_channel_orders.insert(
                                settings_key,
                                output_order.clone(),
                            );
                            // Re-apply converted native routes to active streams. The
                            // stored logical destinations themselves are unchanged.
                            for cue in &self.cue_list.cues {
                                if cue_owns_audio_stream(cue)
                                    && cue.is_active()
                                    && cue_uses_output_endpoint(
                                        cue,
                                        self.audio.backend,
                                        &output,
                                        &self.audio.current_device_id,
                                    )
                                {
                                    let device = cue.io.output_device.as_deref();
                                    self.audio.send(AudioCommand::SetRouting {
                                        id: cue.id,
                                        routes: self.audio.cue_routes_for_device(
                                            device,
                                            &cue.io.channel_routes,
                                        ),
                                    });
                                    self.audio.send(AudioCommand::SetOutputGains {
                                        id: cue.id,
                                        gains_db: self.audio.cue_gains_for_device(
                                            device,
                                            &cue.io.output_gains_db,
                                        ),
                                    });
                                }
                            }
                            info!("Logical output mapping changed for '{}': {:?} -> {:?}; existing Cue assignments and default stereo pair remain unchanged", output, previous_output_order, output_order);
                            self.save_app_settings();
                        }

                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new("Output channel names and levels").strong(),
                        );
                        ui.label(
                            RichText::new(
                                "Rows use TaSQ logical output order. Driver channel names are shown as reported (or via user aliases). Test and Cue playback resolve the same native channel.",
                            )
                            .color(Theme::text_dim())
                            .size(11.0),
                        );
                        for (logical_index, index) in output_order.iter().copied().enumerate() {
                            let Some(setting) = output_settings.get_mut(index) else {
                                continue;
                            };
                            ui.horizontal(|ui| {
                                output_settings_changed |= ui
                                    .add_sized(
                                        [76.0, 26.0],
                                        egui::Checkbox::new(&mut setting.active, "Active"),
                                    )
                                    .changed();
                                ui.add_sized(
                                    [78.0, 26.0],
                                    egui::Label::new(format!("Output {}", logical_index + 1)),
                                );
                                ui.add_sized(
                                    [118.0, 26.0],
                                    egui::Label::new(
                                        RichText::new(self.audio.output_channel_label(
                                            Some(&output),
                                            index,
                                        ))
                                        .color(Theme::text_dim())
                                        .size(11.0),
                                    ),
                                );
                                let name_response = ui.add_sized(
                                    [132.0, 26.0],
                                    egui::TextEdit::singleline(&mut output_names[index])
                                        .hint_text("Custom alias"),
                                );
                                output_names_changed |= name_response.changed();
                                output_names_commit |= name_response.lost_focus()
                                    || (name_response.has_focus()
                                        && ui.input(|input| input.key_pressed(Key::Enter)));
                                if ui
                                    .add_sized(
                                        [58.0, 26.0],
                                        egui::Button::new("Test").fill(Theme::button_bg()),
                                    )
                                    .clicked()
                                {
                                    self.audio
                                        .test_output_channel(&output, logical_index);
                                }
                                let slider_response = ui.add_sized(
                                    [190.0, 26.0],
                                    egui::Slider::new(&mut setting.gain_db, -60.0..=18.0)
                                        .show_value(true)
                                        .custom_formatter(|value, _| {
                                            format!("{:+.1} dB", value)
                                        }),
                                );
                                output_settings_changed |= slider_response.changed();
                            });
                        }
                        if let Some(detected_pair) =
                            self.audio.detected_default_output_pair_for_device(Some(&output))
                        {
                            let detected_left = self
                                .audio
                                .output_channel_label(Some(&output), detected_pair[0]);
                            let detected_right = self
                                .audio
                                .output_channel_label(Some(&output), detected_pair[1]);
                            let current_pair =
                                self.audio.default_output_pair_for_device(Some(&output));
                            let detected_logical_pair = [
                                output_order.iter().position(|&native| native == detected_pair[0]).unwrap_or(0),
                                output_order.iter().position(|&native| native == detected_pair[1]).unwrap_or(1),
                            ];
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "Driver-detected stereo pair: {} / {}",
                                        detected_left, detected_right
                                    ))
                                    .color(Theme::text_dim())
                                    .size(11.0),
                                );
                                if current_pair != detected_logical_pair
                                    && ui.small_button("Use detected pair").clicked()
                                {
                                    self.audio
                                        .set_default_output_pair_for_device(&output, detected_logical_pair);
                                    let settings_key =
                                        self.audio.output_device_settings_key(&output);
                                    self.settings
                                        .default_output_pairs
                                        .insert(settings_key, detected_logical_pair);
                                    self.save_app_settings();
                                }
                            });
                        }

                        if output_settings_changed {
                            self.audio.set_output_channel_settings_for_device(&output, output_settings.clone());
                            let settings_key = self.audio.output_device_settings_key(&output);
                            self.settings
                                .output_channel_settings_by_device
                                .insert(settings_key, output_settings.clone());
                            if output == self.audio.current_device {
                                self.settings.output_channels = output_settings;
                            }
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

                        let logical_pair_options: Vec<(usize, String)> = (0..output_count)
                            .map(|index| (index, format!("TaSQ Output {}", index + 1)))
                            .collect();
                        let mut default_pair =
                            self.audio.default_output_pair_for_device(Some(&output));
                        default_pair[0] = default_pair[0].min(output_count - 1);
                        default_pair[1] = default_pair[1].min(output_count - 1);
                        let mut default_pair_changed = false;
                        ui.horizontal(|ui| {
                            ui.add_sized(
                                [142.0, 28.0],
                                egui::Label::new("Default stereo pair"),
                            );
                            if ui
                                .add_sized(
                                    [174.0, 28.0],
                                    egui::Button::new("Use TaSQ Output 1/2")
                                        .fill(Theme::button_bg()),
                                )
                                .clicked()
                            {
                                if output_order.len() >= 2 {
                                    default_pair = [0, 1];
                                    default_pair_changed = true;
                                }
                            }
                            ui.add_sized([18.0, 26.0], egui::Label::new("L"));
                            egui::ComboBox::from_id_salt((
                                "default_output_left",
                                output.clone(),
                            ))
                            .width(180.0)
                            .selected_text(
                                logical_pair_options
                                    .get(default_pair[0])
                                    .map(|(_, label)| label.as_str())
                                    .unwrap_or("Output 1"),
                            )
                            .show_ui(ui, |ui| {
                                for (index, label) in &logical_pair_options {
                                    default_pair_changed |= ui
                                        .selectable_value(&mut default_pair[0], *index, label)
                                        .changed();
                                }
                            });
                            ui.add_sized([18.0, 26.0], egui::Label::new("R"));
                            egui::ComboBox::from_id_salt((
                                "default_output_right",
                                output.clone(),
                            ))
                            .width(180.0)
                            .selected_text(
                                logical_pair_options
                                    .get(default_pair[1])
                                    .map(|(_, label)| label.as_str())
                                    .unwrap_or("Output 1"),
                            )
                            .show_ui(ui, |ui| {
                                for (index, label) in &logical_pair_options {
                                    default_pair_changed |= ui
                                        .selectable_value(&mut default_pair[1], *index, label)
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
                        if default_pair != [0, 1] {
                            ui.colored_label(
                                egui::Color32::YELLOW,
                                "Default Stereo Pair is applied only to newly imported Audio Cues; existing Cue routes are not modified.",
                            );
                        }

                        ui.label(
                            RichText::new(
                                "I/O Routing can save a backend/device route for each Cue. Until Stage 4, a Cue must use the active backend; ASIO Cues must also use the one currently active ASIO driver.",
                            )
                            .color(Theme::text_dim())
                            .size(10.5),
                        );

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
                        ).color(Theme::text_dim()).size(11.0));

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
                                    .color(Theme::accent()),
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
                                .color(Theme::text_dim())
                                .size(11.0),
                        );
                        if let Some(action) = self.midi_learn_action.clone() {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!("Listening for MIDI input for: {}", action.label()))
                                        .color(Theme::accent())
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
                        let midi_actions = AppAction::MIDI_CONTROL_ACTIONS;
                        let chunk_size = (midi_actions.len() + column_count - 1) / column_count;
                        ui.columns(column_count, |columns| {
                            for (column_index, column_ui) in columns.iter_mut().enumerate() {
                                let start = column_index * chunk_size;
                                let end = (start + chunk_size).min(midi_actions.len());
                                if start >= end {
                                    continue;
                                }
                                for action in &midi_actions[start..end] {
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
                                                .color(Theme::text_dim())
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
                            .color(Theme::text_dim()).size(11.0));
                        if ui.button("Save playback settings").clicked() {
                            self.save_app_settings();
                        }
                    }
                    SettingsTab::BackupLogs => {
                        ui.heading("Project backups");
                        ui.label(
                            RichText::new(
                                "Theatre Show Cue creates a lightweight event snapshot after every successful project save.",
                            )
                            .color(Theme::text_dim())
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
                            RichText::new("0 disables timed backups; backups after manual saves remain enabled.")
                                .color(Theme::text_dim())
                                .size(11.0),
                        );
                        ui.label(
                            RichText::new("The backup folder stores event/project data only, not audio media. Theatre Show Cue keeps the 10 newest snapshots and removes the oldest automatically.")
                                .color(Theme::text_dim())
                                .size(11.0),
                        );

                        if interval_changed {
                            self.last_backup_at = Instant::now();
                            self.save_app_settings();
                        }
                        ui.add_space(14.0);
                        ui.separator();
                        ui.heading("Diagnostic logs");
                        ui.label(
                            RichText::new(
                                "Audio diagnostics and application behaviour are written to separate files. You can export them as one ZIP archive and send it to the developer.",
                            )
                            .color(Theme::text_dim())
                            .size(11.0),
                        );

                        let current_log_directory = crate::diagnostics::active_log_directory()
                            .unwrap_or_else(|| {
                                crate::diagnostics::configured_log_directory(
                                    self.settings.diagnostic_log_directory.as_deref(),
                                )
                            });
                        let mut displayed_log_directory =
                            current_log_directory.display().to_string();
                        let mut selected_log_directory: Option<PathBuf> = None;
                        ui.horizontal(|ui| {
                            ui.label("Save logs to");
                            ui.add_sized(
                                [430.0, 26.0],
                                egui::TextEdit::singleline(&mut displayed_log_directory)
                                    .interactive(false),
                            );
                            if ui.button("Browse…").clicked() {
                                if let Some(path) = FileDialog::new()
                                    .set_directory(&current_log_directory)
                                    .pick_folder()
                                {
                                    selected_log_directory = Some(path);
                                }
                            }
                        });

                        if let Some(path) = selected_log_directory {
                            match crate::diagnostics::set_log_directory(&path) {
                                Ok(()) => {
                                    self.settings.diagnostic_log_directory =
                                        Some(path.to_string_lossy().into_owned());
                                    self.save_app_settings();
                                    if let Err(error) = crate::diagnostics::write_runtime_report(
                                        self.audio.backend.label(),
                                        &self.audio.output_devices,
                                        &self.audio.input_devices,
                                        self.audio.output_channels,
                                    ) {
                                        warn!("Could not refresh diagnostic report: {error}");
                                    }
                                    info!("Diagnostic log directory changed to '{}'", path.display());
                                    self.status_msg =
                                        format!("Diagnostic logs will be saved to {}", path.display());
                                }
                                Err(error) => {
                                    self.status_msg =
                                        format!("Could not change diagnostic log folder: {error}");
                                    warn!("{}", self.status_msg);
                                }
                            }
                        }

                        ui.horizontal_wrapped(|ui| {
                            if ui.button("Use default folder").clicked() {
                                let path = crate::diagnostics::default_log_directory();
                                match crate::diagnostics::set_log_directory(&path) {
                                    Ok(()) => {
                                        self.settings.diagnostic_log_directory = None;
                                        self.save_app_settings();
                                        let _ = crate::diagnostics::write_runtime_report(
                                            self.audio.backend.label(),
                                            &self.audio.output_devices,
                                            &self.audio.input_devices,
                                            self.audio.output_channels,
                                        );
                                        info!("Diagnostic log directory reset to default");
                                        self.status_msg = "Using the default diagnostic log folder".to_string();
                                    }
                                    Err(error) => {
                                        self.status_msg =
                                            format!("Could not reset diagnostic log folder: {error}");
                                        warn!("{}", self.status_msg);
                                    }
                                }
                            }
                            if ui.button("Open log folder").clicked() {
                                if let Err(error) =
                                    crate::diagnostics::open_log_directory(&current_log_directory)
                                {
                                    self.status_msg = format!("Could not open log folder: {error}");
                                    warn!("{}", self.status_msg);
                                }
                            }
                            if ui.button("Export diagnostic ZIP…").clicked() {
                                if let Some(destination) = FileDialog::new()
                                    .add_filter("ZIP archive", &["zip"])
                                    .set_directory(&current_log_directory)
                                    .set_file_name("TheatreShowCue_diagnostics.zip")
                                    .save_file()
                                {
                                    match crate::diagnostics::create_diagnostic_archive(
                                        &current_log_directory,
                                        &destination,
                                    ) {
                                        Ok(file_count) => {
                                            self.status_msg = format!(
                                                "Diagnostic archive created: {} ({} files)",
                                                destination.display(),
                                                file_count
                                            );
                                            info!("{}", self.status_msg);
                                        }
                                        Err(error) => {
                                            self.status_msg =
                                                format!("Could not export diagnostic archive: {error:#}");
                                            warn!("{}", self.status_msg);
                                        }
                                    }
                                }
                            }
                        });

                        match crate::diagnostics::diagnostic_file_counts(&current_log_directory) {
                            Ok((audio_count, application_count, report_count)) => {
                                ui.label(
                                    RichText::new(format!(
                                        "Current folder: {audio_count}/30 audio logs · {application_count}/30 application logs · {report_count}/30 diagnostic reports"
                                    ))
                                    .color(Theme::text_dim())
                                    .size(11.0),
                                );
                            }
                            Err(error) => {
                                ui.label(
                                    RichText::new(format!("Could not read log folder: {error}"))
                                        .color(Theme::text_dim())
                                        .size(11.0),
                                );
                            }
                        }

                        ui.label(
                            RichText::new(
                                "Automatic cleanup keeps the newest 30 files of each type. Only Theatre Show Cue diagnostic files are removed; unrelated files are never deleted. The export contains logs and the diagnostic report, not audio media or project files.",
                            )
                            .color(Theme::text_dim())
                            .size(11.0),
                        );

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
                    .fill(Theme::bg_panel())
                    .stroke(egui::Stroke::new(2.0_f32, Theme::accent()))
                    .inner_margin(egui::Margin::symmetric(28_i8, 18_i8))
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new("Drop audio files to add them to the project")
                                    .color(Theme::text())
                                    .size(18.0)
                                    .strong(),
                            );
                            ui.label(
                                RichText::new("WAV · MP3 · FLAC · OGG · AIFF · M4A")
                                    .color(Theme::text_dim())
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
                        self.report.finish();
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
                        address: "/tasq/event".to_string(),
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
                if ui.button("💬 Add Comment Cue").clicked() {
                    self.add_typed_cue(CueType::Comment {
                        text: String::new(),
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
                if ui.button("Trim").clicked() {
                    self.active_panel = Panel::Trim;
                    ui.close_menu();
                }
                if ui.button("Session Report").clicked() {
                    self.active_panel = Panel::Report;
                    ui.close_menu();
                }
                ui.separator();
                if ui
                    .checkbox(&mut self.notes_sidebar_open, "Notes Sidebar")
                    .changed()
                {
                    ui.close_menu();
                }
            });
        });
    }

    // ── Нижняя панель (Inspector / Waveform / Playback / I/O Routing / MIDI) ──

    fn bottom_panel(&mut self, ui: &mut egui::Ui) {
        // This content is hosted by a real TopBottomPanel. egui reserves its
        // rectangle from the central work area, preventing pointer fall-through.

        let selected_type = self
            .selected
            .and_then(|id| self.cue_list.find(id))
            .map(|cue| cue.cue_type.clone());
        let selected_is_audio = matches!(selected_type.as_ref(), Some(CueType::Audio));
        let selected_is_midi = matches!(selected_type.as_ref(), Some(CueType::Midi { .. }));

        if !selected_is_audio
            && matches!(
                self.active_panel,
                Panel::Waveform | Panel::Playback | Panel::IoRouting | Panel::Trim
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
                ui.selectable_value(&mut self.active_panel, Panel::Trim, "🎛 Trim");
            }
            if selected_is_midi {
                ui.selectable_value(&mut self.active_panel, Panel::Midi, "🎹 MIDI Settings");
            }
            ui.selectable_value(&mut self.active_panel, Panel::Report, "📋 Report");
        });
        ui.separator();

        // Use the lower region's current available height, not the saved
        // previous height. This lets the content scroll inside the resized area.
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
                                    .color(Theme::text_dim()),
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
                            let (
                                seek_position,
                                volume_automation_changed,
                                time_pitch_changed,
                                volume_automation_enabled,
                                volume_automation_points,
                                time_pitch_settings,
                            ) = if let Some(cue) = self.cue_list.find_mut(id) {
                                let (seek, volume_changed, time_pitch_changed) =
                                    WaveformEditor::show(ui, cue);
                                (
                                    seek,
                                    volume_changed,
                                    time_pitch_changed,
                                    cue.volume_automation_enabled,
                                    cue.volume_automation.clone(),
                                    cue.time_pitch.clone(),
                                )
                            } else {
                                (None, false, false, false, Vec::new(), Default::default())
                            };
                            if volume_automation_changed {
                                self.audio.send(AudioCommand::SetVolumeAutomation {
                                    id,
                                    enabled: volume_automation_enabled,
                                    points: volume_automation_points,
                                });
                            }
                            if time_pitch_changed {
                                self.audio.send(AudioCommand::SetTimePitch {
                                    id,
                                    settings: time_pitch_settings,
                                });
                            }
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
                                info!(
                                    "Waveform seek: cue_id={}, position={:.3}s, was_active={}",
                                    id, position_secs, was_active
                                );
                                self.audio.send(AudioCommand::Seek { id, position_secs });
                                self.status_msg =
                                    format!("⏸ Position set to {:.2}s", position_secs);
                            }
                        } else {
                            ui.label(
                                egui::RichText::new("Select an audio cue to edit waveform & loop")
                                    .color(Theme::text_dim()),
                            );
                        }
                    }

                    Panel::Playback => {
                        if let Some(id) = self.selected {
                            let master = self.master_vol;
                            let mut volume_update = None;
                            let mut time_pitch_update = None;
                            if let Some(cue) = self.cue_list.find_mut(id) {
                                let (volume_changed, time_pitch_changed) =
                                    Inspector::show_playback(ui, cue);
                                if volume_changed {
                                    volume_update = Some(cue.volume * master);
                                }
                                if time_pitch_changed {
                                    time_pitch_update = Some(cue.time_pitch.clone());
                                }
                            }
                            if let Some(volume) = volume_update {
                                self.audio.send(AudioCommand::SetVolume(id, volume));
                            }
                            if let Some(settings) = time_pitch_update {
                                self.audio.send(AudioCommand::SetTimePitch { id, settings });
                            }
                        } else {
                            ui.label(
                                egui::RichText::new("Select a cue to configure Playback & Fades")
                                    .color(Theme::text_dim()),
                            );
                        }
                    }

                    Panel::IoRouting => {
                        let selected_id = self.selected;
                        let previous_endpoint = selected_id.and_then(|id| {
                            self.cue_list.find(id).map(|cue| {
                                (
                                    cue.io.output_device.clone(),
                                    cue.io.output_backend
                                        .or_else(|| {
                                            cue.io.output_device_id.as_deref()
                                                .and_then(|id| self.audio.device_registry.find_by_id(id))
                                                .map(|endpoint| endpoint.backend)
                                        })
                                        .unwrap_or(self.audio.backend),
                                    cue.io.output_device_id.clone(),
                                )
                            })
                        });
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
                                let cue_update = self.cue_list.find(id).map(|cue| {
                                    let route_backend = cue
                                        .io
                                        .output_backend
                                        .or_else(|| {
                                            cue.io.output_device_id.as_deref()
                                                .and_then(|id| self.audio.device_registry.find_by_id(id))
                                                .map(|endpoint| endpoint.backend)
                                        })
                                        .unwrap_or(self.audio.backend);
                                    let endpoint_changed = previous_endpoint.as_ref().is_some_and(
                                        |(previous_device, previous_backend, previous_id)| {
                                            let identity_changed = match (
                                                previous_id.as_deref(),
                                                cue.io.output_device_id.as_deref(),
                                            ) {
                                                (Some(previous), Some(current)) => previous != current,
                                                (Some(_), None) => true,
                                                (None, Some(_)) => false,
                                                (None, None) => previous_device != &cue.io.output_device,
                                            };
                                            identity_changed || *previous_backend != route_backend
                                        },
                                    );
                                    (
                                        cue.io.channel_routes.clone(),
                                        cue.io.output_gains_db.clone(),
                                        cue.volume * self.master_vol,
                                        endpoint_changed,
                                        cue.io.output_device.is_some()
                                            && route_backend != self.audio.backend,
                                        route_backend,
                                        cue.name.clone(),
                                    )
                                });
                                if let Some((
                                    routes,
                                    output_gains_db,
                                    volume,
                                    endpoint_changed,
                                    cross_backend_route,
                                    route_backend,
                                    cue_name,
                                )) = cue_update
                                {
                                    if endpoint_changed {
                                        self.stop_target_and_descendants(id);
                                        self.status_msg = if cross_backend_route {
                                            format!(
                                                "Cue '{}' is assigned to {}. It will play after Stage 4 enables cross-backend workers.",
                                                cue_name,
                                                route_backend.short_label()
                                            )
                                        } else {
                                            format!(
                                                "Cue '{}' stopped because its output endpoint changed. Press GO to play through the new endpoint.",
                                                cue_name
                                            )
                                        };
                                    }
                                    let device_name = self.cue_list.find(id)
                                        .and_then(|cue| cue.io.output_device.as_deref());
                                    self.audio.send(AudioCommand::SetRouting {
                                        id,
                                        routes: self.audio.cue_routes_for_device(device_name, &routes),
                                    });
                                    self.audio.send(AudioCommand::SetOutputGains {
                                        id,
                                        gains_db: self.audio.cue_gains_for_device(
                                            device_name,
                                            &output_gains_db,
                                        ),
                                    });
                                    self.audio.send(AudioCommand::SetVolume(id, volume));
                                }
                            }
                        }
                    }

                    Panel::Trim => {
                        let selected_id = self.selected;
                        let trim_changed = if let Some(id) = selected_id {
                            if let Some(cue) = self.cue_list.find_mut(id) {
                                Inspector::show_source_channel_trims(ui, Some(cue), &self.audio)
                            } else {
                                Inspector::show_source_channel_trims(ui, None, &self.audio)
                            }
                        } else {
                            Inspector::show_source_channel_trims(ui, None, &self.audio)
                        };

                        if trim_changed {
                            if let Some(id) = selected_id {
                                if let Some((routes, volume)) = self.cue_list.find(id).map(|cue| {
                                    (cue.io.channel_routes.clone(), cue.volume * self.master_vol)
                                }) {
                                    let device_name = self.cue_list.find(id)
                                        .and_then(|cue| cue.io.output_device.as_deref());
                                    self.audio.send(AudioCommand::SetRouting {
                                        id,
                                        routes: self.audio.cue_routes_for_device(device_name, &routes),
                                    });
                                    // The Trim tab's master strip controls the cue's
                                    // actual playback level, so propagate it live just
                                    // like the Playback and I/O Routing master faders.
                                    self.audio.send(AudioCommand::SetVolume(id, volume));
                                }
                            }
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
                                    .color(Theme::text_dim()),
                            )
                            .wrap(),
                        );
                    }
                }
            });
    }
}

impl eframe::App for TheatreShowCueApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        // Re-apply our palette during update as well as at startup. This keeps
        // egui's native-window defaults from overriding custom text colors on Windows.
        Theme::apply(ctx, &self.settings.appearance);

        if !self.startup_frame_logged {
            info!("Theatre Show Cue GUI reached its first frame");
            self.startup_frame_logged = true;
        }

        // Keep the inspector inside the application viewport and reserve at
        // least roughly 38% of the window for the Cue List, even after resizing.
        let screen_height = ctx.input(|input| input.screen_rect().height());
        let max_bottom_panel_height = if screen_height > 0.0 {
            ((screen_height - 130.0).max(80.0) * 0.55)
                .min(700.0)
                .max(80.0)
        } else {
            600.0
        };
        let clamped_panel_height = self
            .bottom_panel_height
            .clamp(80.0, max_bottom_panel_height);
        if (clamped_panel_height - self.bottom_panel_height).abs() > 1.0 {
            self.bottom_panel_height = clamped_panel_height;
            self.settings.bottom_panel_height = clamped_panel_height;
            self.save_app_settings();
        }

        while let Some(scan) = self.device_registry_monitor.try_recv() {
            let changes = self.audio.device_registry.apply_scan(scan);
            if changes.changed() {
                info!(
                    "Audio device registry changed: +{} new, {} reconnected, {} disconnected, {} renamed",
                    changes.added,
                    changes.reconnected,
                    changes.disconnected,
                    changes.renamed
                );
                let active_endpoint_available = self
                    .audio
                    .device_registry
                    .find_available_by_id(&self.audio.current_device_id)
                    .is_some();
                if !active_endpoint_available && !self.active_output_needs_reopen {
                    self.active_output_needs_reopen = true;
                    self.stop_all();
                    let message = format!(
                        "Active audio endpoint {} — '{}' was disconnected. Playback was stopped; reconnect it and select it again to reopen the stream.",
                        self.audio.backend.short_label(),
                        self.audio.current_device
                    );
                    self.status_msg = message.clone();
                    warn!("{}", message);
                } else if active_endpoint_available && self.active_output_needs_reopen {
                    self.status_msg = format!(
                        "{} — '{}' reconnected. Select it in Audio Settings to reopen the output stream.",
                        self.audio.backend.short_label(),
                        self.audio.current_device
                    );
                } else if changes.added > 0 || changes.reconnected > 0 {
                    self.status_msg = format!(
                        "Audio devices updated: {} available endpoint(s)",
                        self.audio.device_registry.available_count()
                    );
                }
            }
        }

        self.poll_pending_triggers();
        self.poll_time_triggers();
        self.poll_audio();
        self.poll_wait_progress();
        self.poll_midi();
        self.poll_osc();
        self.poll_audio_file_metadata();
        self.poll_periodic_backup();
        self.handle_keyboard(ctx);
        self.handle_drop(ctx);

        if self.cue_list.active_count() > 0 || !self.pending_triggers.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(30));
        } else if self.cue_list.cues.iter().any(|cue| {
            cue.time_trigger_enabled
                && !matches!(&cue.cue_type, CueType::Comment { .. })
                && (chrono::NaiveTime::parse_from_str(&cue.time_trigger_time, "%H:%M").is_ok()
                    || chrono::NaiveTime::parse_from_str(&cue.time_trigger_time, "%H:%M:%S")
                        .is_ok())
        }) {
            // Poll clock-based triggers frequently enough not to miss the scheduled minute/second.
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if (self.settings.backup_interval_minutes > 0 && self.project_path.is_some())
            || self
                .cue_list
                .cues
                .iter()
                .any(|cue| matches!(&cue.cue_type, CueType::Audio) && !cue.file_path.is_file())
        {
            // Timed backups and missing-media recovery are checked once per
            // second even while the show is otherwise idle.
            ctx.request_repaint_after(Duration::from_secs(1));
        } else {
            // The device registry is polled in the background; wake the UI at
            // low frequency so new hot-plug snapshots are consumed promptly.
            ctx.request_repaint_after(Duration::from_secs(1));
        }

        // ── Верхнее меню ──────────────────────────────────────────────────
        egui::TopBottomPanel::top("menu")
            .frame(
                egui::Frame::NONE
                    .fill(Theme::bg_panel())
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
                    .fill(Theme::bg_dark())
                    .inner_margin(egui::Margin::symmetric(8_i8, 2_i8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&self.status_msg)
                            .color(Theme::text_dim())
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
                            .color(Theme::text_dim())
                            .size(11.0),
                        );
                    });
                });
            });

        // ── Панель транспорта ─────────────────────────────────────────────
        egui::TopBottomPanel::bottom("transport")
            .frame(
                egui::Frame::NONE
                    .fill(Theme::bg_panel())
                    .inner_margin(egui::Margin::symmetric(8_i8, 6_i8)),
            )
            .show(ctx, |ui| {
                ui.add_enabled_ui(!self.settings_open, |ui| {
                    let active = self.cue_list.active_count();
                    let prev_master = self.master_vol;
                    let action = TransportBar::show(
                        ui,
                        active,
                        &mut self.master_vol,
                        &mut self.bottom_panel_visible,
                    );
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

        // IMPORTANT: egui requires every TopBottomPanel to be declared before
        // SidePanel and CentralPanel. If the inspector is declared after the
        // Notes SidePanel, egui can paint it over the central area without
        // reserving its height from Cue List. That leaves virtual cue rows
        // underneath the inspector interactive. Keep this panel here, before
        // Notes Sidebar, so CentralPanel receives only the remaining rectangle.
        // The inspector is a real egui panel, not a child painted over the Cue List.
        // Its rectangle is removed from the central work area, so rows below it
        // cannot receive clicks, selection, drag, or resize input.
        let mut inspector_resize_handle: Option<egui::Rect> = None;
        if self.bottom_panel_visible && !self.settings_open {
            let lower_panel = egui::TopBottomPanel::bottom("cue_inspector_panel")
                .resizable(true)
                .height_range(80.0..=max_bottom_panel_height)
                .default_height(self.bottom_panel_height)
                .frame(
                    egui::Frame::NONE
                        .fill(Theme::bg_panel())
                        .inner_margin(egui::Margin::same(8_i8)),
                )
                .show(ctx, |ui| {
                    self.bottom_panel(ui);
                });

            let panel_rect = lower_panel.response.rect;
            // egui's TopBottomPanel resize grip sits on the top edge of the
            // panel. Track the primary drag while it starts in this narrow
            // strip; keep CueTable input disabled until the pointer is released.
            inspector_resize_handle = Some(egui::Rect::from_min_max(
                egui::pos2(panel_rect.left(), panel_rect.top() - 7.0),
                egui::pos2(panel_rect.right(), panel_rect.top() + 7.0),
            ));

            let actual_height = panel_rect.height();
            if (actual_height - self.bottom_panel_height).abs() > 1.0 {
                // A changing panel height while the primary pointer is down is
                // definitive evidence that the splitter owns this drag, even if
                // egui's resize grip is a few pixels outside the reported rect.
                if ctx.input(|input| input.pointer.button_down(egui::PointerButton::Primary)) {
                    self.bottom_panel_resize_active = true;
                }
                self.bottom_panel_height = actual_height;
                self.settings.bottom_panel_height = actual_height;
                if ctx.input(|input| input.pointer.any_released()) {
                    self.save_app_settings();
                }
            }
        }

        let (primary_down, primary_released, pointer_pos) = ctx.input(|input| {
            (
                input.pointer.button_down(egui::PointerButton::Primary),
                input.pointer.button_released(egui::PointerButton::Primary),
                input.pointer.interact_pos(),
            )
        });
        if primary_down
            && pointer_pos
                .zip(inspector_resize_handle)
                .map(|(position, handle)| handle.contains(position))
                .unwrap_or(false)
        {
            self.bottom_panel_resize_active = true;
        }

        if self.bottom_panel_resize_active && primary_released {
            self.bottom_panel_resize_active = false;
            self.bottom_panel_resize_release_guard = true;
        } else if self.bottom_panel_resize_release_guard && !primary_down {
            // The release event is delivered for one frame. Clear the guard on
            // the following frame, after CueTable has safely ignored mouse-up.
            self.bottom_panel_resize_release_guard = false;
        }

        if inspector_resize_handle.is_none() {
            self.bottom_panel_resize_active = false;
            self.bottom_panel_resize_release_guard = false;
        }

        // ── Боковая панель Notes выбранного события ────────────────────────
        if self.notes_sidebar_open && !self.settings_open {
            egui::SidePanel::right("cue_notes_sidebar")
                .resizable(true)
                .default_width(235.0)
                .width_range(150.0..=520.0)
                .frame(
                    egui::Frame::NONE
                        .fill(Theme::bg_panel())
                        .inner_margin(egui::Margin::same(8_i8)),
                )
                .show(ctx, |ui| {
                    // SidePanel reserves its full rectangle from CentralPanel, so
                    // clicks in Notes cannot fall through to the Cue List.
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
                            ui.label(egui::RichText::new(&cue.name).strong().color(Theme::text()));
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
                    .fill(Theme::bg_dark())
                    .inner_margin(egui::Margin::same(0_i8)),
            )
            .show(ctx, |ui| {
                let central_available = ui.available_size();
                ui.spacing_mut().item_spacing.y = 0.0;

                // TopBottomPanel has already reserved the inspector rectangle.
                // The Cue List is built only inside the remaining central rect.
                ui.allocate_ui_with_layout(
                    central_available,
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_clip_rect(ui.max_rect());
                        if self.settings_open {
                            let blocked_rect = ui.max_rect();
                            ui.painter()
                                .rect_filled(blocked_rect, 0.0, Theme::bg_dark());
                            ui.allocate_rect(blocked_rect, egui::Sense::click_and_drag());
                        } else {
                            let selected_cue_info = self.selected.and_then(|selected_id| {
                                self.cue_list.find(selected_id).map(|cue| {
                                    (
                                        cue.number.clone(),
                                        cue.name.clone(),
                                        cue.cue_type.label().to_string(),
                                        cue.color.to_egui(),
                                    )
                                })
                            });

                            // Row one: project title and the exact event currently armed
                            // by selection. Row two: creation tools, slightly lower.
                            ui.vertical(|ui| {
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

                                        let enter_pressed =
                                            ui.input(|input| input.key_pressed(Key::Enter));
                                        let escape_pressed =
                                            ui.input(|input| input.key_pressed(Key::Escape));
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
                                                    .color(Theme::text())
                                                    .size(14.0),
                                            )
                                            .sense(egui::Sense::click()),
                                        );
                                        let title_response = title_response
                                            .on_hover_text("Double-click to rename project");
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

                                    ui.add_space(10.0);
                                    ui.separator();
                                    ui.add_space(8.0);
                                    // Always reserve the color-marker slot and the
                                    // same name/type widths. Selection changes must not
                                    // shift the toolbar's horizontal layout.
                                    let (marker_rect, _) = ui.allocate_exact_size(
                                        egui::vec2(16.0, 20.0),
                                        egui::Sense::hover(),
                                    );
                                    if let Some((number, name, cue_type, cue_color)) =
                                        &selected_cue_info
                                    {
                                        ui.painter().circle_filled(
                                            marker_rect.center(),
                                            5.5,
                                            *cue_color,
                                        );
                                        ui.add_sized(
                                            [270.0, 20.0],
                                            egui::Label::new(
                                                egui::RichText::new(format!(
                                                    "#{} · {}",
                                                    number, name
                                                ))
                                                .strong()
                                                .color(Theme::text())
                                                .size(13.0),
                                            ),
                                        )
                                        .on_hover_text(name.as_str());
                                        ui.add_sized(
                                            [70.0, 20.0],
                                            egui::Label::new(
                                                egui::RichText::new(cue_type)
                                                    .color(Theme::text_dim())
                                                    .size(11.0),
                                            ),
                                        );
                                    } else {
                                        ui.add_sized(
                                            [270.0, 20.0],
                                            egui::Label::new(
                                                egui::RichText::new("No event selected")
                                                    .color(Theme::text_dim())
                                                    .size(12.0),
                                            ),
                                        );
                                        ui.add_sized([70.0, 20.0], egui::Label::new(""));
                                    }
                                });

                                ui.add_space(3.0);
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    ui.add_space(8.0);
                                    // Keep the event creation controls distinct.
                                    // The previous 47 px buttons visually ran together
                                    // on narrower windows; 42 px is a 10% reduction.
                                    let button_size = [42.0, 42.0];
                                    let button_fill = Theme::button_bg();
                                    if ui
                                        .add_sized(
                                            button_size,
                                            egui::Button::new("🔊").fill(button_fill),
                                        )
                                        .on_hover_text("Add audio file(s)")
                                        .clicked()
                                    {
                                        self.add_files();
                                    }
                                    if ui
                                        .add_sized(
                                            button_size,
                                            egui::Button::new("▶").fill(button_fill),
                                        )
                                        .on_hover_text("Add Play cue")
                                        .clicked()
                                    {
                                        self.add_typed_cue(CueType::Play { target_id: None });
                                    }
                                    if ui
                                        .add_sized(
                                            button_size,
                                            egui::Button::new("⏸").fill(button_fill),
                                        )
                                        .on_hover_text("Add Pause cue")
                                        .clicked()
                                    {
                                        self.add_typed_cue(CueType::Pause { target_id: None });
                                    }
                                    if ui
                                        .add_sized(
                                            button_size,
                                            egui::Button::new("⏹").fill(button_fill),
                                        )
                                        .on_hover_text("Add Stop cue")
                                        .clicked()
                                    {
                                        self.add_typed_cue(CueType::Stop { target_id: None });
                                    }
                                    if ui
                                        .add_sized(
                                            button_size,
                                            egui::Button::new("↗").fill(button_fill),
                                        )
                                        .on_hover_text("Add Fade cue")
                                        .clicked()
                                    {
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
                                    if ui
                                        .add_sized(
                                            button_size,
                                            egui::Button::new("🎹").fill(button_fill),
                                        )
                                        .on_hover_text("Add MIDI cue")
                                        .clicked()
                                    {
                                        self.add_typed_cue(CueType::Midi {
                                            channel: 1,
                                            message: MidiMessageKind::NoteOn {
                                                note: 60,
                                                velocity: 100,
                                            },
                                            raw_hex: "90 3C 64".to_string(),
                                        });
                                    }
                                    if ui
                                        .add_sized(
                                            button_size,
                                            egui::Button::new("📡").fill(button_fill),
                                        )
                                        .on_hover_text("Add OSC cue")
                                        .clicked()
                                    {
                                        self.add_typed_cue(CueType::Osc {
                                            address: "/tasq/event".to_string(),
                                            args: Vec::new(),
                                        });
                                    }
                                    if ui
                                        .add_sized(
                                            button_size,
                                            egui::Button::new("📁").fill(button_fill),
                                        )
                                        .on_hover_text("Add Group cue")
                                        .clicked()
                                    {
                                        self.add_typed_cue(CueType::Group {
                                            mode: GroupMode::Normal,
                                            children: Vec::new(),
                                            fade_in_secs: 0.0,
                                            fade_out_secs: 0.0,
                                        });
                                    }
                                    if ui
                                        .add_sized(
                                            button_size,
                                            egui::Button::new("💬").fill(button_fill),
                                        )
                                        .on_hover_text("Add Comment cue")
                                        .clicked()
                                    {
                                        self.add_typed_cue(CueType::Comment {
                                            text: String::new(),
                                        });
                                    }

                                    ui.add_space(6.0);
                                    let notes_width = (ui.available_width() - 120.0).max(120.0);
                                    if let Some(selected_id) = self.selected {
                                        if let Some(cue) = self.cue_list.find_mut(selected_id) {
                                            ui.add_sized(
                                                [notes_width, 47.0],
                                                egui::TextEdit::multiline(&mut cue.notes)
                                                    .desired_rows(2)
                                                    .hint_text("Notes for selected event…"),
                                            );
                                        } else {
                                            ui.add_sized(
                                                [notes_width, 47.0],
                                                egui::Label::new(
                                                    egui::RichText::new(
                                                        "Select an event to edit notes",
                                                    )
                                                    .color(Theme::text_dim()),
                                                ),
                                            );
                                        }
                                    } else {
                                        ui.add_sized(
                                            [notes_width, 47.0],
                                            egui::Label::new(
                                                egui::RichText::new(
                                                    "Select an event to edit notes",
                                                )
                                                .color(Theme::text_dim()),
                                            ),
                                        );
                                    }

                                    if ui
                                        .add_sized(
                                            [42.0, 42.0],
                                            egui::Button::new("📝").fill(button_fill),
                                        )
                                        .on_hover_text("Notes sidebar")
                                        .clicked()
                                    {
                                        self.notes_sidebar_open = !self.notes_sidebar_open;
                                    }
                                    if ui
                                        .add_sized(
                                            [42.0, 42.0],
                                            egui::Button::new("⚙").fill(button_fill),
                                        )
                                        .on_hover_text("Settings")
                                        .clicked()
                                    {
                                        self.settings_open = true;
                                    }
                                });
                            });
                            ui.add(egui::Separator::default().horizontal());

                            // CueTable fills the remaining central work area; the
                            // lower inspector is a separate reserved egui panel.
                            let interactions_enabled = !self.bottom_panel_resize_active
                                && !self.bottom_panel_resize_release_guard;
                            let output_issues = self.cue_output_issues_for_table();
                            let action = ui
                                .add_enabled_ui(interactions_enabled, |ui| {
                                    CueTable::show(
                                        ui,
                                        &mut self.cue_list.cues,
                                        &output_issues,
                                        &mut self.selected,
                                        &mut self.selected_ids,
                                        &mut self.selection_anchor,
                                        &mut self.editing,
                                        &mut self.editing_focus_pending,
                                        &mut self.cue_column_widths,
                                        &mut self.expanded_groups,
                                        self.scroll_to_cue,
                                        interactions_enabled,
                                    )
                                })
                                .inner;
                            self.scroll_to_cue = None;
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
                                    CueTableAction::Copy(id) => {
                                        if !self.selected_ids.contains(&id) {
                                            self.select_only(id);
                                        }
                                        self.copy_selected_cues();
                                    }
                                    CueTableAction::Paste(id) => {
                                        self.select_only(id);
                                        self.paste_cues_after(Some(id));
                                    }
                                    CueTableAction::Play(id) => {
                                        self.select_only(id);
                                        if self.is_comment_cue(id) {
                                            if let Some(next_id) = self.next_non_comment_after(id) {
                                                self.select_only(next_id);
                                            }
                                        } else {
                                            self.trigger_cue(id);
                                        }
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
                                        if self.selected_ids.contains(&id) {
                                            self.dispatch_app_action(AppAction::DeleteSelected);
                                        } else {
                                            self.delete_cue(id);
                                        }
                                    }
                                    CueTableAction::MoveUp(id) => {
                                        if !self.selected_ids.contains(&id) {
                                            self.select_only(id);
                                        }
                                        self.move_selected_cues(true);
                                    }
                                    CueTableAction::MoveDown(id) => {
                                        if !self.selected_ids.contains(&id) {
                                            self.select_only(id);
                                        }
                                        self.move_selected_cues(false);
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
                                            EditField::Duration => {
                                                if let Ok(value) =
                                                    new_val.trim().replace(',', ".").parse::<f64>()
                                                {
                                                    if value.is_finite() && value >= 0.0 {
                                                        if let Some(cue) =
                                                            self.cue_list.find_mut(id)
                                                        {
                                                            if let CueType::Fade {
                                                                duration_secs,
                                                                ..
                                                            } = &mut cue.cue_type
                                                            {
                                                                *duration_secs = value;
                                                                cue.duration_secs = value;
                                                                cue.position_secs =
                                                                    cue.position_secs.min(value);
                                                            }
                                                        }
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
                                        if let Some(target_cue) = self.cue_list.find_mut(target_id)
                                        {
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
                    },
                );
            });
        self.show_settings_window(ctx);
        self.show_drop_overlay(ctx);
    }
}

#[cfg(test)]
mod normal_group_tests {
    use super::{normal_group_manual_stop_in, TheatreShowCueApp};
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

    fn comment_cue() -> Cue {
        Cue::new_typed(CueType::Comment {
            text: "Stage direction".to_string(),
        })
    }

    #[test]
    fn go_on_an_active_audio_cue_restarts_at_its_start_marker() {
        let mut cue = Cue::new_typed(CueType::Audio);
        cue.start_secs = 3.0;
        cue.duration_secs = 40.0;
        cue.position_secs = 18.0;
        cue.state = crate::cue::CueState::Playing;

        assert_eq!(TheatreShowCueApp::cue_play_start_position(&cue), 3.0);

        cue.state = crate::cue::CueState::Stopped;
        assert_eq!(TheatreShowCueApp::cue_play_start_position(&cue), 18.0);
    }

    #[test]
    fn comment_rows_do_not_create_a_manual_boundary_in_normal_groups() {
        let first = audio_cue(ContinueMode::AutoFollow);
        let comment = comment_cue();
        let manual = audio_cue(ContinueMode::Nothing);
        let after = audio_cue(ContinueMode::Nothing);
        let children = vec![first, comment, manual.clone(), after];
        let group = normal_group(&children);
        let mut cues = children;
        cues.push(group.clone());

        assert_eq!(
            normal_group_manual_stop_in(&cues, group.id, &mut HashSet::new()),
            Some((vec![group.id], manual.id))
        );
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

#[cfg(test)]
mod audio_output_mapping_tests {
    use super::{cue_uses_output_endpoint, normalize_buffer_selection};
    use crate::config::AudioBackend;
    use crate::cue::{Cue, CueType};

    #[test]
    fn generic_default_pair_migration_converts_native_to_logical_only_once() {
        let mut settings = crate::config::AppSettings::default();
        settings.audio_routing_schema = 0;
        let device = "ASIO::Six Channel Interface".to_string();
        settings
            .output_channel_orders
            .insert(device.clone(), vec![2, 3, 4, 5, 0, 1]);
        settings.default_output_pairs.insert(device.clone(), [0, 1]);

        assert!(super::migrate_saved_stereo_pairs_to_logical(&mut settings));
        assert_eq!(settings.default_output_pairs[&device], [4, 5]);
        assert_eq!(settings.audio_routing_schema, 3);
        assert!(!super::migrate_saved_stereo_pairs_to_logical(&mut settings));
        assert_eq!(settings.default_output_pairs[&device], [4, 5]);
    }

    #[test]
    fn buffer_selection_respects_backend_control_model() {
        let wasapi_options = [256, 512, 1_024, 2_048, 4_096, 8_192];
        let directsound_options = [1_024, 2_048, 4_096, 8_192, 16_384];
        let asio_options = [128, 256, 512];

        assert_eq!(
            normalize_buffer_selection(AudioBackend::Wasapi, Some(480), &wasapi_options),
            Some(480)
        );
        assert_eq!(
            normalize_buffer_selection(AudioBackend::DirectSound, Some(480), &directsound_options),
            None
        );
        assert_eq!(
            normalize_buffer_selection(AudioBackend::Asio, Some(1_024), &asio_options),
            None
        );
        assert_eq!(
            normalize_buffer_selection(AudioBackend::Asio, Some(512), &asio_options),
            Some(512)
        );
    }

    #[test]
    fn global_cue_route_follows_the_active_output_endpoint() {
        let cue = Cue::new_typed(CueType::Audio);
        assert!(cue_uses_output_endpoint(
            &cue,
            AudioBackend::Asio,
            "US-322 / US-366",
            "asio:tascam"
        ));
    }

    #[test]
    fn explicit_cue_route_matches_stable_endpoint_identity() {
        let mut cue = Cue::new_typed(CueType::Audio);
        cue.io.output_backend = Some(AudioBackend::Asio);
        cue.io.output_device = Some("TASCAM US-366".to_string());
        cue.io.output_device_id = Some("asio:tascam".to_string());

        assert!(cue_uses_output_endpoint(
            &cue,
            AudioBackend::Asio,
            "US-322 / US-366",
            "asio:tascam"
        ));
        assert!(!cue_uses_output_endpoint(
            &cue,
            AudioBackend::Wasapi,
            "US-322 / US-366",
            "asio:tascam"
        ));
    }
}
