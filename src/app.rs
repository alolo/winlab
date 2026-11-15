// src/app.rs — главное состояние приложения Theatre Show Cue

use crate::audio::device_registry::DeviceRegistryMonitor;
use crate::audio::engine::{AudioCommand, AudioEngine};
use crate::audio::waveform::{
    analyze_audio_file, audio_analysis_cache_stats, AudioAnalysisCacheStats,
    WaveformAnalysisManager,
};
use crate::audio::worker::AudioWorkerManagerEvent;
use crate::config::{
    AppAction, AppSettings, AudioBackend, AudioPerformanceSettings, IconOverride,
    MidiControlBinding, MidiControlKind, ThemePreset, UiDesign, UiScalePreset, MAX_RECENT_PROJECTS,
};
use crate::cue::{
    ContinueMode, Cue, CueList, CueState, CueType, FadeCurve, GroupMode, MidiMessageKind,
    OscCueArg, WaitPhase,
};
use crate::dispatch::DispatchTiming;
use crate::export::session_report::SessionReport;
use crate::midi::input::{MidiDispatch, MidiInputManager, MidiOutputManager};
use crate::osc::server::{OscDispatch, OscServer};
use crate::performance::SystemCpuMonitor;
use crate::project::{
    asset_manager::AssetManager,
    save_load::{Project, ProjectAudioRouting},
};
use crate::ui::{
    audio_log::AudioLogViewer,
    cue_table::{visible_cue_ids, visible_navigation_cue_ids, CueTable, CueTableAction, EditField},
    icons::{
        icon_button, icon_selectable_label, import_custom_icon, toolbar_icon_colors, IconManager,
        BUILTIN_ICON_KEYS, TOOLBAR_ICON_ROLES,
    },
    inspector::{Inspector, InspectorAction},
    theme::Theme,
    transport::{TransportAction, TransportBar},
    waveform_editor::WaveformEditor,
};

use eframe::egui::{self, Context, Id, Key, RichText};
use rfd::FileDialog;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

const INTERNAL_CUE_CLIPBOARD_MARKER: &str = "TheatreShowCue::InternalCueClipboard::v1";
const WAVEFORM_BUCKETS: usize = 4096;
static SLOW_GUI_UPDATE_LOG_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    let left = left.canonicalize().unwrap_or_else(|_| left.to_path_buf());
    let right = right.canonicalize().unwrap_or_else(|_| right.to_path_buf());

    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn remember_recent_project_path(recent_projects: &mut Vec<String>, path: &Path) {
    let normalized = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    recent_projects
        .retain(|existing| !paths_refer_to_same_location(Path::new(existing), &normalized));
    recent_projects.insert(0, normalized.to_string_lossy().into_owned());
    recent_projects.truncate(MAX_RECENT_PROJECTS);
}

fn recent_project_menu_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn recent_project_hover_path(path: &Path) -> String {
    let display = path.to_string_lossy();

    if let Some(unc_path) = display.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc_path}")
    } else if let Some(normal_path) = display.strip_prefix(r"\\?\") {
        normal_path.to_string()
    } else {
        display.into_owned()
    }
}

fn serialize_project_snapshot(project: &Project, cue_list: &CueList) -> Option<Vec<u8>> {
    let mut snapshot = project.clone();
    snapshot.cue_list = cue_list.clone();
    serde_json::to_vec(&snapshot).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClosePromptKind {
    ExistingProject,
    NewProject,
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

    if let Some(saved_id) = cue.io.output_device_id.as_deref() {
        // A stable ID is authoritative. Never let a matching display name
        // redirect a Cue to a different physical endpoint.
        return saved_id == endpoint_id;
    }

    match cue.io.output_device.as_deref() {
        None => true,
        Some(saved_name) => saved_name == device_name,
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

/// Configure default channel routes once source-channel metadata is available.
/// This helper is shared by synchronous imports and background project analysis.
fn ensure_audio_cue_routes(audio: &AudioEngine, cue: &mut Cue) {
    if cue.audio_channels == 0 {
        return;
    }

    let route_backend = cue
        .io
        .output_backend
        .or_else(|| {
            cue.io
                .output_device_id
                .as_deref()
                .and_then(|id| audio.device_registry.find_by_id(id))
                .map(|endpoint| endpoint.backend)
        })
        .unwrap_or(audio.backend);
    let route_device_name = cue
        .io
        .output_device
        .clone()
        .or_else(|| {
            cue.io
                .output_device_id
                .as_deref()
                .and_then(|id| audio.device_registry.find_by_id(id))
                .filter(|endpoint| endpoint.backend == route_backend)
                .map(|endpoint| endpoint.device_name.clone())
        })
        .unwrap_or_else(|| audio.current_device.clone());
    let registered_route_id = if cue.io.output_device_id.is_none() && cue.io.output_device.is_some()
    {
        audio.registered_endpoint_id(route_backend, &route_device_name)
    } else {
        None
    };
    let route_endpoint_id = cue
        .io
        .output_device_id
        .as_deref()
        .or(registered_route_id.as_deref())
        .or_else(|| {
            cue.io
                .output_device
                .is_none()
                .then_some(audio.current_device_id.as_str())
        });
    let output_count = audio.output_channels_for_endpoint(
        route_backend,
        route_endpoint_id,
        Some(&route_device_name),
    );
    let default_route_channels = audio.default_route_channels_for_source_endpoint(
        route_backend,
        route_endpoint_id,
        Some(&route_device_name),
        cue.audio_channels,
    );
    cue.io.ensure_channel_routes_with_defaults(
        cue.audio_channels,
        output_count,
        &default_route_channels,
    );
}

fn waveform_path_key(path: &Path) -> PathBuf {
    let normalized = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    #[cfg(windows)]
    {
        PathBuf::from(normalized.to_string_lossy().to_lowercase())
    }
    #[cfg(not(windows))]
    {
        normalized
    }
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
    StartAllCuesSegment,
    AdvanceAllCues,
}

#[derive(Debug, Clone)]
struct PendingTrigger {
    cue_id: Uuid,
    /// Cue that scheduled this trigger, used to cancel its automatic chain on Stop/Pause.
    source_cue_id: Option<Uuid>,
    fire_at: Instant,
    action: PendingAction,
    /// Original GO timestamp, retained across Pre/Post-Wait and group segments
    /// so end-to-end GO latency remains measurable after scheduled delays.
    go_received_at: Option<Instant>,
}

#[derive(Debug, Clone)]
struct ActivePlaylist {
    group_id: Uuid,
    children: Vec<Uuid>,
    current_idx: usize,
    fade_in_secs: f64,
    fade_out_secs: f64,
}

#[derive(Debug, Clone, Default)]
struct AllCuesSegment {
    /// Cues in this segment start in the same launch transaction.
    children: Vec<Uuid>,
    /// Delay before the first Cue in this segment starts.
    pre_wait_secs: f64,
    /// Delay after the last Cue starts before the next segment is released.
    post_wait_secs: f64,
}

#[derive(Debug, Clone)]
struct ActiveAllCues {
    group_id: Uuid,
    segments: Vec<AllCuesSegment>,
    next_segment_idx: usize,
    remaining: HashSet<Uuid>,
    fade_in_secs: f64,
    fade_out_secs: f64,
}

/// Build consecutive All Cues launch segments. A non-zero pre-wait starts
/// a new segment; a non-zero post-wait closes the current segment. Therefore
/// every segment contains only Cue events that may start in one transaction.
fn build_all_cues_segments(cue_list: &CueList, children: &[Uuid]) -> Vec<AllCuesSegment> {
    let mut segments = Vec::new();
    let mut current = AllCuesSegment::default();

    for child_id in children {
        let Some(cue) = cue_list.find(*child_id) else {
            continue;
        };
        let pre_wait = cue.pre_wait_secs.max(0.0);
        let post_wait = cue.post_wait_secs.max(0.0);

        if pre_wait > 0.0 && !current.children.is_empty() {
            segments.push(std::mem::take(&mut current));
        }

        if current.children.is_empty() {
            current.pre_wait_secs = pre_wait;
        }

        current.children.push(*child_id);

        if post_wait > 0.0 {
            current.post_wait_secs = post_wait;
            segments.push(std::mem::take(&mut current));
        }
    }

    if !current.children.is_empty() {
        segments.push(current);
    }

    segments
}

/// Returns whether Cue completion itself owns the post-wait countdown.
///
/// Auto-Continue schedules its post-wait from the Cue's trigger time, so an
/// audio completion arriving after that timer has elapsed must not start the
/// same post-wait a second time. Playlist and All Cues children retain their
/// existing group-owned completion path; their sequencing rules are separate
/// from the normal Auto-Continue chain.
fn completion_owns_post_wait(
    continue_mode: &ContinueMode,
    is_playlist_child: bool,
    is_all_cues_child: bool,
    is_managed_group_cue: bool,
) -> bool {
    if is_playlist_child || is_all_cues_child || is_managed_group_cue {
        true
    } else {
        *continue_mode != ContinueMode::AutoContinue
    }
}
/// Calculates how long Cue completion must still wait.
///
/// When another scheduler owns the post-wait, completion may wait for its
/// remaining time, but must never recreate an already elapsed countdown.
fn completion_post_wait_delay(
    extra_delay: f64,
    post_wait: f64,
    existing_post_wait_remaining: Option<f64>,
    completion_owns_wait: bool,
) -> f64 {
    let extra_delay = extra_delay.max(0.0);
    if let Some(remaining) = existing_post_wait_remaining {
        let remaining = remaining.max(0.0);
        if completion_owns_wait {
            remaining
        } else {
            extra_delay.max(remaining)
        }
    } else if completion_owns_wait {
        extra_delay + post_wait.max(0.0)
    } else {
        extra_delay
    }
}

/// Find the next Cue that requires manual GO after a Normal group's initial
/// child and all reachable Auto-Continue / Auto-Follow transitions.
///
/// ContinueMode belongs to the Cue whose field is set: it controls how the
/// successor is triggered, not how this Cue itself was triggered. The child with
/// ContinueMode::Nothing has already been started: either it was the first child
/// launched by GO on the group, or it was reached automatically from its
/// predecessor. Therefore, when another playable child follows, that *next*
/// child is the one that must be selected for manual GO. A Nothing on the final
/// child means the automatic chain has consumed the whole group.
fn normal_group_next_manual_cue_in(
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
            if let Some((mut path, next_manual_id)) =
                normal_group_next_manual_cue_in(cues, child.id, visited)
            {
                path.insert(0, group_id);
                return Some((path, next_manual_id));
            }
        }

        if child.continue_mode == ContinueMode::Nothing {
            if let Some(next_child) = valid_children.get(index + 1) {
                return Some((vec![group_id], next_child.id));
            }
            return None;
        }
    }
    None
}

pub struct TheatreShowCueApp {
    audio: AudioEngine,
    icons: IconManager,
    device_registry_monitor: DeviceRegistryMonitor,
    /// True while the global endpoint worker is missing or awaiting recovery.
    /// The worker manager clears this after automatic or manual restoration.
    active_output_needs_reopen: bool,
    midi: MidiInputManager,
    midi_out: MidiOutputManager,
    osc: Option<OscServer>,

    cue_list: CueList,
    project: Project,
    project_path: Option<PathBuf>,
    project_needs_save_as: bool,
    /// Serialized project state at the last successful open/save. Runtime-only
    /// Cue state is excluded by the Cue serde model.
    saved_project_snapshot: Option<Vec<u8>>,
    close_prompt: Option<ClosePromptKind>,
    close_allowed: bool,
    assets: AssetManager,
    report: SessionReport,
    settings: AppSettings,
    settings_open: bool,
    settings_tab: SettingsTab,
    hotkey_capture: Option<AppAction>,
    /// Cue-specific keyboard capture is stored by stable Cue ID.
    cue_hotkey_capture: Option<Uuid>,
    audio_log_viewer: AudioLogViewer,
    midi_learn_action: Option<AppAction>,
    midi_cc_pressed: HashSet<(u8, u8)>,

    selected: Option<Uuid>,
    /// One-shot Cue List scroll target used by navigation and the next-manual-Cue
    /// selection performed after GO starts an automatic sequence.
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
    /// Latest output peak across all active endpoint workers (linear amplitude).
    output_meter_peak: f32,
    /// System-wide CPU usage sampled once per second outside the audio thread.
    cpu_monitor: SystemCpuMonitor,
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
    /// Nestable boundary for one logical GO/Auto-Continue launch. The outermost
    /// scope gives Audio, MIDI and OSC dispatches one shared transaction ID;
    /// each backend receives its own ordered batch when the scope is closed.
    launch_transaction_depth: usize,
    launch_transaction_id: Option<Uuid>,
    launch_transaction_started_at: Option<Instant>,
    /// GO timestamp captured at the operator/control entry point. It is moved
    /// into the outermost transaction when that transaction begins.
    pending_go_received_at: Option<Instant>,
    launch_transaction_go_received_at: Option<Instant>,
    /// Original GO origin for active Cues whose completion may happen later
    /// (for example, an Audio Cue in Playlist or an Auto-Follow chain).
    cue_go_origins: HashMap<Uuid, Instant>,
    pending_midi_dispatches: Vec<MidiDispatch>,
    pending_osc_dispatches: Vec<OscDispatch>,
    immediate_playlist_advances: VecDeque<Uuid>,
    /// Background waveform workers never block the GUI or audio callback.
    waveform_manager: WaveformAnalysisManager,
    waveform_generation: u64,
    waveform_pending_cues: HashSet<Uuid>,
    waveform_scheduled_cues: HashSet<Uuid>,
    waveform_total_cues: usize,
    waveform_cache_stats_before: AudioAnalysisCacheStats,
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
        let mut asio_profiles_sanitized = false;
        for (key, profile) in &mut settings.audio_performance_settings_by_device {
            if key.starts_with("ASIO::") && *profile != AudioPerformanceSettings::default() {
                *profile = AudioPerformanceSettings::default();
                asio_profiles_sanitized = true;
            }
        }
        let legacy_performance = AudioPerformanceSettings::for_backend(
            requested_backend,
            AudioPerformanceSettings {
                sample_rate: settings.audio_sample_rate,
                buffer_size_frames: settings.audio_buffer_size_frames,
            },
        );
        let saved_performance = settings
            .audio_output_device
            .as_deref()
            .and_then(|device_name| {
                let key = AudioEngine::output_device_settings_key_for_backend(
                    requested_backend,
                    device_name,
                );
                settings
                    .audio_performance_settings_by_device
                    .get(&key)
                    .copied()
            })
            .unwrap_or(legacy_performance);
        let requested_performance =
            AudioPerformanceSettings::for_backend(requested_backend, saved_performance);
        Theme::apply(&cc.egui_ctx, &settings.appearance);
        let icons = IconManager::new(&cc.egui_ctx);
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
        let active_performance_key = AudioEngine::output_device_settings_key_for_backend(
            audio.backend,
            &audio.current_device,
        );
        let active_performance_profile_missing = !settings
            .audio_performance_settings_by_device
            .contains_key(&active_performance_key);
        let legacy_performance_mirror_changed = settings.audio_sample_rate
            != audio.performance_settings.sample_rate
            || settings.audio_buffer_size_frames != audio.performance_settings.buffer_size_frames;
        settings
            .audio_performance_settings_by_device
            .insert(active_performance_key, audio.performance_settings);
        settings.audio_sample_rate = audio.performance_settings.sample_rate;
        settings.audio_buffer_size_frames = audio.performance_settings.buffer_size_frames;

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
        let active_output_settings_key = audio.output_device_settings_key(&audio.current_device);
        let active_output_settings_profile_missing = !settings
            .output_channel_settings_by_device
            .contains_key(&active_output_settings_key);
        let initial_output_settings = settings.output_channels.clone();
        settings
            .output_channel_settings_by_device
            .entry(active_output_settings_key)
            .or_insert(initial_output_settings);
        audio.set_endpoint_preferences(
            settings.audio_performance_settings_by_device.clone(),
            settings.output_channel_settings_by_device.clone(),
        );
        if active_performance_profile_missing
            || active_output_settings_profile_missing
            || legacy_performance_mirror_changed
            || asio_profiles_sanitized
        {
            if let Err(error) = settings.save() {
                warn!("Could not persist migrated endpoint preferences: {error}");
            }
        }
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
            Duration::from_secs(60),
            audio.device_registry.snapshot_endpoints(),
        );

        let mut midi = MidiInputManager::new_with_preferred(settings.midi_input_port.as_deref());
        midi.set_repaint_context(cc.egui_ctx.clone());
        let mut midi_out =
            MidiOutputManager::new_with_preferred(settings.midi_output_port.as_deref());
        midi_out.set_repaint_context(cc.egui_ctx.clone());

        let osc = OscServer::new(settings.osc_listen_port, settings.osc_send_addr.clone())
            .map_err(|e| warn!("OSC unavailable: {e}"))
            .ok();
        let cue_column_widths = settings.cue_column_widths;
        let bottom_panel_height = settings.bottom_panel_height.clamp(80.0, 1200.0);
        let initial_project_audio_routing = ProjectAudioRouting::from_maps(
            settings.output_channel_orders.clone(),
            settings.default_output_pairs.clone(),
        );
        let initial_cue_list = CueList::new("Main");
        let initial_project = Project::with_audio_routing(initial_project_audio_routing);
        let saved_project_snapshot =
            serialize_project_snapshot(&initial_project, &initial_cue_list);

        let waveform_manager = WaveformAnalysisManager::new(1);
        let waveform_generation = waveform_manager.begin_generation();
        let waveform_cache_stats_before = audio_analysis_cache_stats();

        info!("Theatre Show Cue initialization complete; creating the main window");

        Self {
            audio,
            icons,
            device_registry_monitor,
            active_output_needs_reopen: false,
            midi,
            midi_out,
            osc,
            cue_list: initial_cue_list,
            project: initial_project,
            project_path: None,
            project_needs_save_as: false,
            saved_project_snapshot,
            close_prompt: None,
            close_allowed: false,
            assets: AssetManager::new(),
            report: SessionReport::new("Untitled Project"),
            settings,
            settings_open: false,
            settings_tab: SettingsTab::Audio,
            hotkey_capture: None,
            cue_hotkey_capture: None,
            audio_log_viewer: AudioLogViewer::default(),
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
            output_meter_peak: 0.0,
            cpu_monitor: SystemCpuMonitor::new(),
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
            launch_transaction_depth: 0,
            launch_transaction_id: None,
            launch_transaction_started_at: None,
            pending_go_received_at: None,
            launch_transaction_go_received_at: None,
            cue_go_origins: HashMap::new(),
            pending_midi_dispatches: Vec::new(),
            pending_osc_dispatches: Vec::new(),
            immediate_playlist_advances: VecDeque::new(),
            waveform_manager,
            waveform_generation,
            waveform_pending_cues: HashSet::new(),
            waveform_scheduled_cues: HashSet::new(),
            waveform_total_cues: 0,
            waveform_cache_stats_before,
        }
    }

    // ── Запуск и диспетчеризация всех типов событий ──────────────────────────

    fn go(&mut self) {
        let go_received_at = Instant::now();

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
        let is_normal_group = self.cue_list.find(id).is_some_and(|cue| {
            matches!(
                &cue.cue_type,
                CueType::Group {
                    mode: GroupMode::Normal,
                    ..
                }
            )
        });

        self.record_go_received_at(go_received_at);
        self.trigger_cue(id);
        // Defer this log write until after the dispatch batches have been queued;
        // the monotonic timestamp itself was captured at the very entry to GO.
        info!(
            "GO received timestamp captured: cue_id={}, capture_to_log_us={}",
            id,
            go_received_at.elapsed().as_micros()
        );

        // Normal Groups may select an internal manual boundary and expand the
        // required nested groups while starting. Preserve that specialized
        // selection, but make the Cue List follow it.
        if is_normal_group && self.selected != Some(id) {
            if let Some(selected_id) = self.selected {
                self.scroll_to_cue = Some(selected_id);
                if let Some(cue) = self.cue_list.find(selected_id) {
                    info!(
                        "GO selected Normal Group manual boundary: cue_id={}, number='{}', name='{}'",
                        cue.id, cue.number, cue.name
                    );
                }
            }
            return;
        }

        // Playlist and All Cues select the next manual Cue after the whole group,
        // not whichever child happens to be starting or completing right now.
        // The same target is used for scrolling so delayed automatic events do
        // not pull the viewport through the queue one Cue at a time.
        if let Some(next_id) = next_manual {
            self.select_only(next_id);
            self.scroll_to_cue = Some(next_id);
            if let Some(cue) = self.cue_list.find(next_id) {
                info!(
                    "GO selected next manual Cue after automatic sequence: cue_id={}, number='{}', name='{}'",
                    cue.id, cue.number, cue.name
                );
            }
        } else {
            self.selected = None;
            self.selected_ids.clear();
            self.selection_anchor = None;
            self.editing = None;
            self.editing_focus_pending = false;
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

    fn record_go_received_at(&mut self, go_received_at: Instant) {
        if self.launch_transaction_depth > 0 {
            // A GO can arrive inside an already-open transaction (for example,
            // from a MIDI control binding processed in the same input poll).
            self.launch_transaction_go_received_at
                .get_or_insert(go_received_at);
        } else {
            self.pending_go_received_at = Some(go_received_at);
        }
    }

    fn begin_launch_transaction(&mut self) {
        if self.launch_transaction_depth == 0 {
            let transaction_id = Uuid::new_v4();
            let transaction_started_at = Instant::now();
            let go_received_at = self.pending_go_received_at.take();
            self.launch_transaction_id = Some(transaction_id);
            self.launch_transaction_started_at = Some(transaction_started_at);
            self.launch_transaction_go_received_at = go_received_at;
            self.audio
                .send(AudioCommand::BeginLaunchTransaction { transaction_id });
            info!(
                "Launch transaction stage: transaction_id={} stage=TRANSACTION_STARTED go_to_transaction_started_us={:?} transaction_started_to_go_received_us={:?}",
                transaction_id,
                go_received_at.and_then(|go_at| transaction_started_at
                    .checked_duration_since(go_at)
                    .map(|elapsed| elapsed.as_micros())),
                go_received_at.and_then(|go_at| go_at
                    .checked_duration_since(transaction_started_at)
                    .map(|elapsed| elapsed.as_micros()))
            );
        }
        self.launch_transaction_depth = self.launch_transaction_depth.saturating_add(1);
    }

    fn end_launch_transaction(&mut self) {
        if self.launch_transaction_depth == 0 {
            return;
        }

        // Advance zero-duration Playlist items iteratively before closing the
        // outer transaction. This removes one GUI-frame delay per MIDI/OSC Cue
        // while preserving the configured Playlist order.
        if self.launch_transaction_depth == 1 {
            self.drain_immediate_playlist_advances();
        }

        self.launch_transaction_depth -= 1;
        if self.launch_transaction_depth == 0 {
            let transaction_started_at = self
                .launch_transaction_started_at
                .take()
                .unwrap_or_else(Instant::now);
            let dispatch_timing = DispatchTiming {
                go_received_at: self.launch_transaction_go_received_at.take(),
                transaction_started_at,
            };
            if let Some(transaction_id) = self.launch_transaction_id.take() {
                self.audio
                    .send(AudioCommand::EndLaunchTransaction { transaction_id });

                let midi_events = std::mem::take(&mut self.pending_midi_dispatches);
                let midi_count = midi_events.len();
                if !midi_events.is_empty() {
                    if let Err(error) =
                        self.midi_out
                            .send_batch(transaction_id, dispatch_timing, midi_events)
                    {
                        tracing::error!(
                            target: "TaSQ::midi::input",
                            "Could not submit MIDI dispatch batch: transaction_id={}, error={}",
                            transaction_id,
                            error
                        );
                    }
                }

                let osc_events = std::mem::take(&mut self.pending_osc_dispatches);
                let osc_count = osc_events.len();
                if !osc_events.is_empty() {
                    if let Some(server) = &self.osc {
                        if let Err(error) =
                            server.send_batch(transaction_id, dispatch_timing, osc_events)
                        {
                            tracing::error!(
                                target: "TaSQ::osc::server",
                                "Could not submit OSC dispatch batch: transaction_id={}, error={}",
                                transaction_id,
                                error
                            );
                        }
                    } else {
                        tracing::error!(
                            target: "TaSQ::osc::server",
                            "OSC dispatch batch dropped because OSC server is unavailable: transaction_id={}, events={}",
                            transaction_id,
                            osc_count
                        );
                    }
                }

                info!(
                    "Launch transaction completed: transaction_id={}, midi_events={}, osc_events={}",
                    transaction_id,
                    midi_count,
                    osc_count
                );
            }
        }
    }

    fn drain_immediate_playlist_advances(&mut self) {
        const MAX_IMMEDIATE_ADVANCES_PER_TRANSACTION: usize = 2048;
        let mut processed = 0usize;

        while let Some(cue_id) = self.immediate_playlist_advances.pop_front() {
            processed += 1;
            if processed > MAX_IMMEDIATE_ADVANCES_PER_TRANSACTION {
                warn!(
                    "Immediate Playlist advance limit reached; deferring remaining {} Cue(s) to the next update",
                    self.immediate_playlist_advances.len() + 1
                );
                self.pending_triggers.push(PendingTrigger {
                    cue_id,
                    source_cue_id: None,
                    fire_at: Instant::now(),
                    action: PendingAction::TriggerCue,
                    go_received_at: self.launch_transaction_go_received_at,
                });
                while let Some(next_id) = self.immediate_playlist_advances.pop_front() {
                    self.pending_triggers.push(PendingTrigger {
                        cue_id: next_id,
                        source_cue_id: None,
                        fire_at: Instant::now(),
                        action: PendingAction::TriggerCue,
                        go_received_at: self.launch_transaction_go_received_at,
                    });
                }
                break;
            }
            self.trigger_cue(cue_id);
        }
    }

    /// Public trigger boundary. Nested automatic chains and groups share the
    /// outer transaction; only the outermost call emits Begin/End markers.
    fn trigger_cue(&mut self, id: Uuid) {
        if self.cue_list.find(id).is_none() {
            return;
        }
        self.begin_launch_transaction();
        self.trigger_cue_inner(id);
        self.end_launch_transaction();
    }

    fn trigger_cue_inner(&mut self, id: Uuid) {
        self.trigger_cue_inner_with_pre_wait(id, true);
    }

    /// Trigger a Cue whose pre-wait has already elapsed. All Cues groups use
    /// this path after their sequence scheduler releases a delayed segment.
    fn trigger_cue_inner_with_pre_wait(&mut self, id: Uuid, honor_pre_wait: bool) {
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

        if let Some(go_origin) = self.launch_transaction_go_received_at {
            self.cue_go_origins.insert(id, go_origin);
        } else {
            self.cue_go_origins.remove(&id);
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
        let trigger_started_at = self
            .launch_transaction_started_at
            .unwrap_or_else(Instant::now);
        info!(
            "Cue trigger requested: id={}, number='{}', name='{}', state={:?}, pre_wait={:.3}s, post_wait={:.3}s",
            cue.id,
            cue.number,
            cue.name,
            cue.state,
            cue.pre_wait_secs,
            cue.post_wait_secs
        );
        let pre_wait = if honor_pre_wait {
            cue.pre_wait_secs.max(0.0)
        } else {
            0.0
        };
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
            let wait_started_at = trigger_started_at;
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
                go_received_at: self.launch_transaction_go_received_at,
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
                let now = trigger_started_at;
                let delay = pre_wait + post_wait;
                if post_wait > 0.0 {
                    self.pending_triggers.push(PendingTrigger {
                        cue_id: id,
                        source_cue_id: Some(id),
                        fire_at: now + std::time::Duration::from_secs_f64(pre_wait),
                        action: PendingAction::BeginPostWait,
                        go_received_at: self.launch_transaction_go_received_at,
                    });
                    self.pending_triggers.push(PendingTrigger {
                        cue_id: id,
                        source_cue_id: Some(id),
                        fire_at: now + std::time::Duration::from_secs_f64(delay),
                        action: PendingAction::ClearWait,
                        go_received_at: self.launch_transaction_go_received_at,
                    });
                }
                if delay > 0.0 {
                    self.pending_triggers.push(PendingTrigger {
                        cue_id: next_id,
                        source_cue_id: Some(id),
                        fire_at: now + std::time::Duration::from_secs_f64(delay),
                        action: PendingAction::TriggerCue,
                        go_received_at: self.launch_transaction_go_received_at,
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

    /// Schedule Cue completion after any post-wait owned by completion.
    /// `extra_delay` is used by timed actions such as Fade. Normal Auto-Continue
    /// owns its post-wait from trigger time and must not restart it here.
    fn schedule_cue_completion(&mut self, cue_id: Uuid, extra_delay: f64) {
        let cue_go_origin = self.cue_go_origins.get(&cue_id).copied();
        if self.launch_transaction_depth > 0 && self.launch_transaction_go_received_at.is_none() {
            self.launch_transaction_go_received_at = cue_go_origin;
        }
        let dispatch_go_origin = self.launch_transaction_go_received_at.or(cue_go_origin);

        let (post_wait, continue_mode, is_managed_group_cue) = self
            .cue_list
            .find(cue_id)
            .map(|cue| {
                (
                    cue.post_wait_secs.max(0.0),
                    cue.continue_mode.clone(),
                    matches!(
                        &cue.cue_type,
                        CueType::Group {
                            mode: GroupMode::Playlist | GroupMode::AllCues,
                            ..
                        }
                    ),
                )
            })
            .unwrap_or((0.0, ContinueMode::Nothing, false));
        let is_playlist_child = self
            .active_playlists
            .iter()
            .any(|run| run.children.contains(&cue_id));
        let is_all_cues_child = self
            .active_all_cues
            .iter()
            .any(|run| run.remaining.contains(&cue_id));
        let completion_owns_wait = completion_owns_post_wait(
            &continue_mode,
            is_playlist_child,
            is_all_cues_child,
            is_managed_group_cue,
        );
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

        if completion_owns_wait && existing_post_wait_remaining.is_none() && post_wait > 0.0 {
            if let Some(cue) = self.cue_list.find_mut(cue_id) {
                cue.wait_phase = WaitPhase::Post;
                cue.wait_started_at = Some(now + std::time::Duration::from_secs_f64(extra_delay));
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

        let delay = completion_post_wait_delay(
            extra_delay,
            post_wait,
            existing_post_wait_remaining,
            completion_owns_wait,
        );

        if !completion_owns_wait && post_wait > 0.0 {
            debug!(
                "Cue completion will not restart externally managed post-wait: cue_id={}, continue_mode={:?}, playlist_child={}, all_cues_child={}, remaining_wait={:?}",
                cue_id,
                continue_mode,
                is_playlist_child,
                is_all_cues_child,
                existing_post_wait_remaining
            );
        }

        if delay > 0.0 {
            self.pending_triggers.push(PendingTrigger {
                cue_id,
                source_cue_id: Some(cue_id),
                fire_at: now + std::time::Duration::from_secs_f64(delay),
                action: PendingAction::CompleteCue,
                go_received_at: dispatch_go_origin,
            });
        } else {
            let owns_transaction = self.launch_transaction_depth == 0;
            if owns_transaction {
                self.pending_go_received_at = dispatch_go_origin;
                self.begin_launch_transaction();
            }
            self.complete_cue(cue_id);
            if owns_transaction {
                self.end_launch_transaction();
            }
        }
    }

    /// Called after an event has finished and its post-wait has elapsed.
    /// Playlist advances one child; All Cues waits for every child; Normal
    /// follows the cue's Auto-Follow setting.
    fn complete_cue(&mut self, cue_id: Uuid) {
        if let Some(go_origin) = self.cue_go_origins.remove(&cue_id) {
            if self.launch_transaction_depth > 0 {
                self.launch_transaction_go_received_at
                    .get_or_insert(go_origin);
            } else {
                self.pending_go_received_at.get_or_insert(go_origin);
            }
        }
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
                // Queue the next child for iterative draining at the outer
                // launch-transaction boundary. Instant Cue runs therefore do
                // not wait for another GUI frame or recurse through the stack.
                self.apply_group_fades(next_child_id, fade_in, fade_out);
                self.status_msg = format!("📁 Playlist: event {}/{}", next_idx + 1, children.len());
                let already_started_as_fade_target = fade_target_id == Some(next_child_id)
                    && self
                        .cue_list
                        .find(next_child_id)
                        .map(|cue| cue.is_active())
                        .unwrap_or(false);
                if !already_started_as_fade_target {
                    self.immediate_playlist_advances.push_back(next_child_id);
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

    /// Resolve the endpoint context used by both routing calculations and the
    /// worker manager. A Cue without an explicit endpoint uses the active global
    /// endpoint; an explicit Cue retains its own backend and stable device ID.
    fn cue_output_endpoint_context(
        &self,
        cue: &Cue,
    ) -> (AudioBackend, Option<String>, String, bool) {
        let explicit = cue.io.output_device.is_some() || cue.io.output_device_id.is_some();
        if explicit {
            let backend = self.cue_output_backend(cue);
            let name = cue
                .io
                .output_device_id
                .as_deref()
                .and_then(|id| self.audio.device_registry.find_by_id(id))
                .filter(|endpoint| endpoint.backend == backend)
                .map(|endpoint| endpoint.device_name.clone())
                .or_else(|| cue.io.output_device.clone())
                .unwrap_or_else(|| "Unavailable endpoint".to_string());
            let id = cue
                .io
                .output_device_id
                .clone()
                .or_else(|| self.audio.registered_endpoint_id(backend, &name));
            (backend, id, name, true)
        } else {
            (
                self.audio.backend,
                Some(self.audio.current_device_id.clone()),
                self.audio.current_device.clone(),
                false,
            )
        }
    }

    /// Resolve a saved Cue endpoint by stable ID. For older projects, migrate
    /// the legacy (backend, display name) pair to the registry ID when available.
    fn normalize_cue_output_endpoint(&mut self, cue_id: Uuid, cue: &mut Cue) {
        let route_backend = self.cue_output_backend(cue);
        let Some(device_name) = cue.io.output_device.clone().or_else(|| {
            cue.io
                .output_device_id
                .as_deref()
                .and_then(|id| self.audio.device_registry.find_by_id(id))
                .filter(|endpoint| endpoint.backend == route_backend)
                .map(|endpoint| endpoint.device_name.clone())
        }) else {
            return;
        };

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
            // Do not enumerate native output devices while starting a Cue.
            // This function is reached from the playback path (and Audio Cues
            // can pass through it more than once). CPAL device enumeration and
            // capability queries may block for tens of milliseconds, which
            // serializes Auto-Continue starts and creates audible offsets.
            //
            // Endpoint metadata is refreshed when the user selects a device
            // in the Inspector or changes the global output. The device
            // registry already contains the channel metadata needed here.
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
    /// active global output. Explicit offline routes and empty output matrices
    /// are surfaced in the Cue List.
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

        let registered_endpoint_id = if has_explicit_endpoint && cue.io.output_device_id.is_none() {
            self.audio
                .registered_endpoint_id(route_backend, device_name)
        } else {
            None
        };
        let endpoint_id = if has_explicit_endpoint {
            cue.io
                .output_device_id
                .as_deref()
                .or(registered_endpoint_id.as_deref())
        } else {
            Some(self.audio.current_device_id.as_str())
        };
        let output_count =
            self.audio
                .output_channels_for_endpoint(route_backend, endpoint_id, Some(device_name));
        let output_settings = self.audio.output_channel_settings_for_endpoint(
            route_backend,
            endpoint_id,
            Some(device_name),
        );
        let output_order = self.audio.output_channel_display_order_for_endpoint(
            route_backend,
            endpoint_id,
            Some(device_name),
        );
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

    /// Do not start a Cue if its selected endpoint is offline. Backend and
    /// driver isolation are handled by the endpoint worker manager.
    fn cue_output_backend_is_active(&mut self, cue: &Cue) -> bool {
        let (backend, endpoint_id, device_name, explicit) = self.cue_output_endpoint_context(cue);
        let available = if explicit {
            self.audio
                .registered_endpoint_available(backend, endpoint_id.as_deref(), &device_name)
        } else {
            self.audio
                .device_registry
                .find_by_id(&self.audio.current_device_id)
                .map(|endpoint| endpoint.is_available())
                .unwrap_or(self.audio.current_sample_rate > 0 && self.audio.output_channels > 0)
        };

        if !available {
            let message = format!(
                "Cue '{}' is routed to {} — '{}', but that endpoint is currently unavailable.",
                cue.name,
                backend.short_label(),
                device_name
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
        let (route_backend, route_endpoint_id, route_device_name, explicit_endpoint) =
            self.cue_output_endpoint_context(&snapshot);
        let output_channels = self.audio.output_channels_for_endpoint(
            route_backend,
            route_endpoint_id.as_deref(),
            Some(&route_device_name),
        );
        let mut routing = snapshot.io.clone();
        let default_route_channels = self.audio.default_route_channels_for_source_endpoint(
            route_backend,
            route_endpoint_id.as_deref(),
            Some(&route_device_name),
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

        let native_routes = self.audio.cue_routes_for_endpoint(
            route_backend,
            route_endpoint_id.as_deref(),
            Some(&route_device_name),
            &routing.channel_routes,
        );
        let native_gains = self.audio.cue_gains_for_endpoint(
            route_backend,
            route_endpoint_id.as_deref(),
            Some(&route_device_name),
            &routing.output_gains_db,
        );
        info!(
            "Audio Cue routing: id={}, logical_routes={:?}, native_routes={:?}, mapping={:?}",
            snapshot.id,
            routing.channel_routes,
            native_routes,
            self.audio.output_channel_display_order_for_endpoint(
                route_backend,
                route_endpoint_id.as_deref(),
                Some(&route_device_name),
            ),
        );
        let generation = self.bump_audio_generation(snapshot.id);
        self.audio.send(AudioCommand::Play {
            id: snapshot.id,
            generation,
            file: snapshot.file_path.clone(),
            output_device: explicit_endpoint.then_some(route_device_name.clone()),
            output_backend: explicit_endpoint.then_some(route_backend),
            output_device_id: explicit_endpoint
                .then(|| route_endpoint_id.clone())
                .flatten(),
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
        // Automatic Cue execution must not move the Cue List viewport. GO
        // chooses and scrolls to the next manual Cue after the whole chain.
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
            self.cue_go_origins.remove(&id);
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
                    self.cue_go_origins.remove(&id);
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
                    let (route_backend, route_endpoint_id, route_device_name, explicit_endpoint) =
                        self.cue_output_endpoint_context(&cue_snapshot);
                    let track_output_channels = self.audio.output_channels_for_endpoint(
                        route_backend,
                        route_endpoint_id.as_deref(),
                        Some(&route_device_name),
                    );
                    let mut io_routing = cue_snapshot.io.clone();
                    let default_route_channels =
                        self.audio.default_route_channels_for_source_endpoint(
                            route_backend,
                            route_endpoint_id.as_deref(),
                            Some(&route_device_name),
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
                    let native_routes = self.audio.cue_routes_for_endpoint(
                        route_backend,
                        route_endpoint_id.as_deref(),
                        Some(&route_device_name),
                        &io_routing.channel_routes,
                    );
                    let native_gains = self.audio.cue_gains_for_endpoint(
                        route_backend,
                        route_endpoint_id.as_deref(),
                        Some(&route_device_name),
                        &io_routing.output_gains_db,
                    );
                    info!(
                        "Play Cue routing: id={}, logical_routes={:?}, native_routes={:?}, mapping={:?}",
                        cue_snapshot.id,
                        io_routing.channel_routes,
                        native_routes,
                        self.audio.output_channel_display_order_for_endpoint(
                            route_backend,
                            route_endpoint_id.as_deref(),
                            Some(&route_device_name),
                        ),
                    );
                    let generation = self.bump_audio_generation(cue_snapshot.id);
                    self.audio.send(AudioCommand::Play {
                        id: cue_snapshot.id,
                        generation,
                        file: cue_snapshot.file_path.clone(),
                        output_device: explicit_endpoint.then_some(route_device_name.clone()),
                        output_backend: explicit_endpoint.then_some(route_backend),
                        output_device_id: explicit_endpoint
                            .then(|| route_endpoint_id.clone())
                            .flatten(),
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
                self.pending_midi_dispatches.push(MidiDispatch {
                    cue_id: cue_snapshot.id,
                    cue_number: cue_snapshot.number.clone(),
                    cue_name: cue_snapshot.name.clone(),
                    bytes: bytes.clone(),
                });
                self.report
                    .record(&cue_snapshot.number, &cue_snapshot.name, 0.0, "MIDI");
                self.status_msg = format!("🎹 MIDI queued: {:02X?}", bytes);
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
                if self.osc.is_some() {
                    self.pending_osc_dispatches.push(OscDispatch {
                        cue_id: cue_snapshot.id,
                        cue_number: cue_snapshot.number.clone(),
                        cue_name: cue_snapshot.name.clone(),
                        addr: address.clone(),
                        args: osc_args,
                    });
                    self.status_msg = format!("📡 OSC queued: {address}");
                    self.report
                        .record(&cue_snapshot.number, &cue_snapshot.name, 0.0, "OSC");
                } else {
                    tracing::warn!(
                        target: "TaSQ::osc::server",
                        "OSC Cue could not be queued because OSC server is unavailable: cue_id={}, cue_number='{}', address='{}'",
                        cue_snapshot.id,
                        cue_snapshot.number,
                        address
                    );
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
                            if let Some((groups_to_expand, next_manual_id)) =
                                self.normal_group_next_manual_cue(id, &mut HashSet::new())
                            {
                                for group_id in groups_to_expand {
                                    self.expanded_groups.insert(group_id);
                                }
                                // Select the next Cue that requires manual GO. The Cue
                                // with Continue=Nothing has already been started by the
                                // group or reached automatically from its predecessor.
                                self.select_only(next_manual_id);
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
                            let segments = build_all_cues_segments(&self.cue_list, &valid_children);
                            let segment_count = segments.len();
                            self.active_all_cues.push(ActiveAllCues {
                                group_id: id,
                                segments,
                                next_segment_idx: 0,
                                remaining: valid_children.iter().copied().collect(),
                                fade_in_secs,
                                fade_out_secs,
                            });
                            info!(
                                "All Cues group started: group_id={}, children={}, launch_segments={}",
                                id,
                                valid_children.len(),
                                segment_count
                            );
                            self.status_msg =
                                format!("📁 All Cues: starting {} events", valid_children.len());
                            self.advance_all_cues(id);
                        }
                    }
                }
            }
        }
    }

    /// Release the next All Cues segment. Consecutive zero-wait children
    /// share one launch transaction; a pre-wait delays the entire next segment,
    /// and a post-wait delays every segment after the current one.
    fn advance_all_cues(&mut self, group_id: Uuid) {
        let next_segment = self
            .active_all_cues
            .iter_mut()
            .find(|run| run.group_id == group_id)
            .and_then(|run| {
                let segment_index = run.next_segment_idx + 1;
                let segment = run.segments.get(run.next_segment_idx)?.clone();
                let segment_count = run.segments.len();
                run.next_segment_idx += 1;
                Some((
                    segment,
                    run.fade_in_secs,
                    run.fade_out_secs,
                    segment_index,
                    segment_count,
                ))
            });

        let Some((segment, fade_in, fade_out, segment_index, segment_count)) = next_segment else {
            return;
        };

        if segment.children.is_empty() {
            self.advance_all_cues(group_id);
            return;
        }

        if segment.pre_wait_secs > 0.0 {
            let first_child_id = segment.children[0];
            let first_child_already_running = self
                .cue_list
                .find(first_child_id)
                .map(|cue| cue.is_active())
                .unwrap_or(false);
            if first_child_already_running {
                self.start_all_cues_segment(
                    group_id,
                    segment,
                    fade_in,
                    fade_out,
                    segment_index,
                    segment_count,
                );
                return;
            }

            let wait_started_at = Instant::now();
            if let Some(cue) = self.cue_list.find_mut(first_child_id) {
                cue.state = CueState::Waiting;
                cue.position_secs = 0.0;
                cue.wait_phase = WaitPhase::Pre;
                cue.wait_started_at = Some(wait_started_at);
                cue.wait_duration_secs = segment.pre_wait_secs;
            }
            self.pending_triggers.push(PendingTrigger {
                cue_id: first_child_id,
                source_cue_id: Some(group_id),
                fire_at: wait_started_at + Duration::from_secs_f64(segment.pre_wait_secs),
                action: PendingAction::StartAllCuesSegment,
                go_received_at: self.launch_transaction_go_received_at,
            });
            info!(
                "All Cues segment waiting for pre-wait: group_id={}, segment={}/{}, first_cue_id={}, cue_count={}, pre_wait={:.3}s",
                group_id,
                segment_index,
                segment_count,
                first_child_id,
                segment.children.len(),
                segment.pre_wait_secs
            );
            self.status_msg = format!("⏳ All Cues pre-wait ({:.1}s)", segment.pre_wait_secs);
            return;
        }

        self.start_all_cues_segment(
            group_id,
            segment,
            fade_in,
            fade_out,
            segment_index,
            segment_count,
        );
    }

    /// Start all Cues in one zero-wait segment. The group scheduler, rather
    /// than individual child completion, owns the next segment's timing.
    fn start_all_cues_segment(
        &mut self,
        group_id: Uuid,
        segment: AllCuesSegment,
        fade_in: f64,
        fade_out: f64,
        segment_index: usize,
        segment_count: usize,
    ) {
        let run_is_active = self
            .active_all_cues
            .iter()
            .any(|run| run.group_id == group_id);
        if !run_is_active {
            return;
        }

        info!(
            "All Cues segment started: group_id={}, segment={}/{}, cue_count={}, pre_wait={:.3}s, post_wait={:.3}s",
            group_id,
            segment_index,
            segment_count,
            segment.children.len(),
            segment.pre_wait_secs,
            segment.post_wait_secs
        );

        for child_id in &segment.children {
            let already_running = self
                .cue_list
                .find(*child_id)
                .map(|cue| cue.is_active())
                .unwrap_or(false);
            if already_running {
                continue;
            }

            self.apply_group_fades(*child_id, fade_in, fade_out);
            self.trigger_cue_inner_with_pre_wait(*child_id, false);
        }

        if segment.post_wait_secs > 0.0 {
            if let Some(last_child_id) = segment.children.last().copied() {
                self.begin_auto_continue_post_wait(last_child_id);
                let wait_started_at = self
                    .cue_list
                    .find(last_child_id)
                    .and_then(|cue| cue.wait_started_at)
                    .unwrap_or_else(Instant::now);
                self.pending_triggers.push(PendingTrigger {
                    cue_id: group_id,
                    source_cue_id: Some(group_id),
                    fire_at: wait_started_at + Duration::from_secs_f64(segment.post_wait_secs),
                    action: PendingAction::AdvanceAllCues,
                    go_received_at: self.launch_transaction_go_received_at,
                });
                info!(
                    "All Cues segment waiting for post-wait: group_id={}, segment={}/{}, last_cue_id={}, next_segment={}, post_wait={:.3}s",
                    group_id,
                    segment_index,
                    segment_count,
                    last_child_id,
                    segment_index + 1,
                    segment.post_wait_secs
                );
                self.status_msg = format!("⏳ All Cues post-wait ({:.1}s)", segment.post_wait_secs);
                return;
            }
        }

        self.advance_all_cues(group_id);
    }

    /// Start a segment whose first Cue has just completed its pre-wait.
    fn start_all_cues_segment_after_pre_wait(&mut self, group_id: Uuid, first_child_id: Uuid) {
        let segment = self
            .active_all_cues
            .iter()
            .find(|run| run.group_id == group_id)
            .and_then(|run| {
                run.segments
                    .iter()
                    .enumerate()
                    .find(|(_, segment)| segment.children.first() == Some(&first_child_id))
                    .map(|(index, segment)| {
                        (
                            segment.clone(),
                            run.fade_in_secs,
                            run.fade_out_secs,
                            index + 1,
                            run.segments.len(),
                        )
                    })
            });

        if let Some((segment, fade_in, fade_out, segment_index, segment_count)) = segment {
            if let Some(cue) = self.cue_list.find_mut(first_child_id) {
                if cue.state == CueState::Waiting && cue.wait_phase == WaitPhase::Pre {
                    cue.clear_wait_runtime();
                    cue.state = CueState::Stopped;
                    cue.position_secs = 0.0;
                }
            }
            self.start_all_cues_segment(
                group_id,
                segment,
                fade_in,
                fade_out,
                segment_index,
                segment_count,
            );
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

    /// Returns the expanded group path and the next Cue that requires manual GO
    /// after the Normal group's automatic chain reaches a Continue=Nothing Cue.
    fn normal_group_next_manual_cue(
        &self,
        group_id: Uuid,
        visited: &mut HashSet<Uuid>,
    ) -> Option<(Vec<Uuid>, Uuid)> {
        normal_group_next_manual_cue_in(&self.cue_list.cues, group_id, visited)
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
            self.cue_go_origins.remove(id);
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

        if ready.is_empty() {
            return;
        }

        // Group only actions with the exact same scheduled deadline. This keeps
        // simultaneous pre-wait/Auto-Continue actions together without merging
        // unrelated triggers that merely became overdue in the same slow GUI frame.
        ready.sort_by_key(|trigger| trigger.fire_at);
        let mut ready = ready.into_iter().peekable();
        while let Some(first) = ready.next() {
            let group_deadline = first.fire_at;
            let mut same_deadline = vec![first];

            while ready
                .peek()
                .is_some_and(|next| next.fire_at == group_deadline)
            {
                if let Some(trigger) = ready.next() {
                    same_deadline.push(trigger);
                }
            }

            // A single worker batch may contain actions from multiple chains
            // released at the exact same deadline. Use the earliest inherited
            // GO as the transaction's end-to-end origin; each Cue still keeps
            // its own ID and ordering in the dispatch payload.
            self.pending_go_received_at = same_deadline
                .iter()
                .filter_map(|trigger| trigger.go_received_at)
                .min();
            self.begin_launch_transaction();
            for trigger in same_deadline {
                self.process_pending_trigger(trigger);
            }
            self.end_launch_transaction();
        }
    }

    fn process_pending_trigger(&mut self, trigger: PendingTrigger) {
        match trigger.action {
            PendingAction::ExecuteCue => self.execute_cue_now(trigger.cue_id),
            PendingAction::TriggerCue => self.trigger_cue(trigger.cue_id),
            PendingAction::BeginPostWait => self.begin_auto_continue_post_wait(trigger.cue_id),
            PendingAction::ClearWait => self.clear_cue_wait(trigger.cue_id),
            PendingAction::CompleteCue => self.complete_cue(trigger.cue_id),
            PendingAction::StartAllCuesSegment => {
                if let Some(group_id) = trigger.source_cue_id {
                    self.start_all_cues_segment_after_pre_wait(group_id, trigger.cue_id);
                }
            }
            PendingAction::AdvanceAllCues => self.advance_all_cues(trigger.cue_id),
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

        if due.is_empty() {
            return;
        }

        self.begin_launch_transaction();
        for (cue_id, cue_name, fire_key) in due {
            self.time_trigger_last_fired.insert(cue_id, fire_key);
            self.status_msg = format!("⏱ Time trigger: {cue_name}");
            self.trigger_cue(cue_id);
        }
        self.end_launch_transaction();
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
        } else {
            self.cue_go_origins.remove(&id);
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
        self.immediate_playlist_advances.clear();
        self.active_playlists.clear();
        self.active_all_cues.clear();
        self.fade_started_at.clear();
        self.fade_managed_targets.clear();
        self.cue_go_origins.clear();
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

        let analysis = analyze_audio_file(&final_path, WAVEFORM_BUCKETS);
        let mut cue = Cue::new_audio(final_path.clone());
        cue.audio_channels = analysis.channels;
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
        let duration = analysis.duration_secs;
        if duration > 0.0 {
            cue.duration_secs = duration;
            cue.loop_region.end_secs = duration;
        }
        cue.waveform = analysis.waveform.clone();

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
        self.cue_go_origins.remove(&id);
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

    // ── Project-scoped audio routing ─────────────────────────────────────────

    /// Return the active project's routing profile. Legacy projects without
    /// the new field temporarily inherit workstation defaults until saved.
    fn current_project_audio_routing(&self) -> ProjectAudioRouting {
        self.project.audio_routing.clone().unwrap_or_else(|| {
            ProjectAudioRouting::from_maps(
                self.settings.output_channel_orders.clone(),
                self.settings.default_output_pairs.clone(),
            )
        })
    }

    /// Apply the active project's endpoint-specific logical mapping and default
    /// stereo pairs to the audio engine. Other workstation preferences (aliases,
    /// trims, device selection and performance) remain in AppSettings.
    fn apply_project_audio_routing(&mut self) {
        let routing = self.current_project_audio_routing();
        self.audio
            .set_output_channel_orders(routing.output_channel_orders);
        self.audio
            .set_default_output_pairs(routing.default_output_pairs);
    }

    /// Update both the active project and the workstation's template for new
    /// projects. Opening another project never overwrites that template.
    fn store_project_output_channel_order(&mut self, key: String, order: Vec<usize>) {
        self.project
            .audio_routing
            .get_or_insert_with(ProjectAudioRouting::default)
            .output_channel_orders
            .insert(key.clone(), order.clone());
        self.settings.output_channel_orders.insert(key, order);
        self.save_app_settings();
        self.status_msg =
            "Logical Output Mapping changed for this project — save the project to keep it."
                .to_string();
    }

    fn store_project_default_output_pair(&mut self, key: String, pair: [usize; 2]) {
        self.project
            .audio_routing
            .get_or_insert_with(ProjectAudioRouting::default)
            .default_output_pairs
            .insert(key.clone(), pair);
        self.settings.default_output_pairs.insert(key, pair);
        self.save_app_settings();
        self.status_msg =
            "Default Stereo Pair changed for this project — save the project to keep it."
                .to_string();
    }

    // ── Сохранение и загрузка проекта ────────────────────────────────────────

    fn commit_cue_edit(&mut self, id: Uuid, field: EditField, new_val: String) {
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
                    if let Ok(value) = new_val.trim().parse::<f64>() {
                        cue.pre_wait_secs = value.max(0.0);
                    }
                }
            }
            EditField::PostWait => {
                if let Some(cue) = self.cue_list.find_mut(id) {
                    if let Ok(value) = new_val.trim().parse::<f64>() {
                        cue.post_wait_secs = value.max(0.0);
                    }
                }
            }
            EditField::Duration => {
                if let Ok(value) = new_val.trim().replace(',', ".").parse::<f64>() {
                    if value.is_finite() && value >= 0.0 {
                        if let Some(cue) = self.cue_list.find_mut(id) {
                            if let CueType::Fade { duration_secs, .. } = &mut cue.cue_type {
                                *duration_secs = value;
                                cue.duration_secs = value;
                                cue.position_secs = cue.position_secs.min(value);
                            }
                        }
                    }
                }
            }
        }
    }

    fn commit_pending_edits_before_save(&mut self) {
        if let Some((id, field, value)) = self.editing.take() {
            self.commit_cue_edit(id, field, value);
        }
        self.editing_focus_pending = false;

        if let Some(project_name) = self.project_name_edit.take() {
            self.rename_project(project_name);
        }
        self.project_name_edit_focus_pending = false;
    }

    fn project_has_unsaved_changes(&self) -> bool {
        if self.editing.is_some() || self.project_name_edit.is_some() {
            return true;
        }
        match (
            self.saved_project_snapshot.as_deref(),
            serialize_project_snapshot(&self.project, &self.cue_list),
        ) {
            (Some(saved), Some(current)) => saved != current.as_slice(),
            _ => true,
        }
    }

    fn mark_project_snapshot_saved(&mut self) {
        self.project.cue_list = self.cue_list.clone();
        self.saved_project_snapshot = serialize_project_snapshot(&self.project, &self.cue_list);
        if self.saved_project_snapshot.is_none() {
            warn!("Could not serialize the current project snapshot for close confirmation");
        }
    }

    fn request_application_close(&mut self, ctx: &Context) {
        if !ctx.input(|input| input.viewport().close_requested()) || self.close_allowed {
            return;
        }

        // A second native close request while the confirmation is open must
        // not bypass the operator's Save / Don't Save / Cancel decision.
        if self.close_prompt.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            return;
        }

        if self.project_has_unsaved_changes() {
            // Keep the native window alive until the operator chooses an action.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_prompt = Some(if self.project_path.is_some() {
                ClosePromptKind::ExistingProject
            } else {
                ClosePromptKind::NewProject
            });
        } else {
            // Let the original native close request continue when there is
            // nothing to save.
            self.close_allowed = true;
        }
    }

    fn show_close_confirmation(&mut self, ctx: &Context) {
        let Some(kind) = self.close_prompt else {
            return;
        };

        let (title, message) = match kind {
            ClosePromptKind::ExistingProject => (
                "Save changes to this project?",
                "The show has unsaved changes. Save them before closing TaSQ?",
            ),
            ClosePromptKind::NewProject => (
                "Save new project?",
                "The Cue List has unsaved changes. Save the entire project as a bundle before closing TaSQ?",
            ),
        };

        let mut action = None;
        egui::Window::new(title)
            .id(Id::new("close_save_confirmation"))
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(340.0);
                ui.label(message);
                ui.add_space(8.0);
                // Keep the close actions evenly distributed across the dialog:
                // Save on the left, Don't Save centered, and Cancel on the right.
                // Use compact, consistent 6 px padding on all three buttons.
                ui.spacing_mut().button_padding = egui::vec2(6.0, 6.0);
                ui.columns(3, |columns| {
                    columns[0].with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                        if ui.button("Save").clicked() {
                            action = Some(0_u8);
                        }
                    });
                    columns[1].with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                        if ui.button("Don't Save").clicked() {
                            action = Some(1_u8);
                        }
                    });
                    columns[2].with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                        if ui.button("Cancel").clicked() {
                            action = Some(2_u8);
                        }
                    });
                });
            });

        match action {
            Some(0) => {
                self.commit_pending_edits_before_save();
                let saved = match kind {
                    ClosePromptKind::ExistingProject => self.save_project(),
                    ClosePromptKind::NewProject => self.save_as_bundle(),
                };
                if saved {
                    info!("Close confirmation: project saved successfully; closing application");
                    self.close_prompt = None;
                    self.close_allowed = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Some(1) => {
                info!("Close confirmation: operator discarded unsaved project changes");
                self.close_prompt = None;
                self.close_allowed = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Some(2) => {
                self.close_prompt = None;
            }
            _ => {}
        }
    }

    fn save_project(&mut self) -> bool {
        // Renaming an already-open project changes its in-memory identity only.
        // Ctrl+S must therefore create a new file instead of overwriting or
        // renaming the file that was originally opened.
        if self.project_needs_save_as {
            self.status_msg =
                "Project name changed — choose a new project file with Save As".to_string();
            return self.save_as_project();
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
                None => return false,
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
                    return false;
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
                self.remember_recent_project(&path);
                self.status_msg = format!("💾 Saved: {}", path.display());
                self.create_project_backup("save");
                self.mark_project_snapshot_saved();
                true
            }
            Err(e) => {
                self.status_msg = format!("Save error: {e}");
                error!("{e}");
                false
            }
        }
    }

    /// Save the current project under a new filename. Media references are
    /// preserved; use Save as Bundle to copy media into a portable folder.
    fn save_as_project(&mut self) -> bool {
        let default_name = format!("{}.tsq", self.project.settings.name);

        let Some(path) = FileDialog::new()
            .add_filter("Theatre Show Cue Project", &["tsq", "clabs"])
            .set_file_name(&default_name)
            .save_file()
        else {
            return false;
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
            return false;
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
                self.remember_recent_project(&path);
                self.status_msg = format!("💾 Saved as: {}", path.display());
                self.create_project_backup("save-as");
                self.mark_project_snapshot_saved();
                true
            }
            Err(error) => {
                self.status_msg = format!("Save As error: {error}");
                error!("Could not save project copy: {error}");
                false
            }
        }
    }

    /// Create a portable bundle folder:
    /// <chosen-parent>/<project>/<project>.tsq + audio/ and backup/.
    /// The user chooses a parent directory; TaSQ creates one self-contained
    /// project folder inside it instead of asking for a project-file path.
    fn save_as_bundle(&mut self) -> bool {
        let Some(bundle_name) = sanitize_project_name(&self.project.settings.name) else {
            self.status_msg = "Project name cannot be used as a bundle folder name".to_string();
            return false;
        };

        let Some(bundle_parent) = FileDialog::new().pick_folder() else {
            return false;
        };

        let bundle_dir = bundle_parent.join(&bundle_name);
        let bundle_project_path = bundle_dir.join(format!("{bundle_name}.tsq"));
        let audio_dir = bundle_dir.join("audio");
        let is_current_bundle = self
            .project_path
            .as_deref()
            .is_some_and(|path| paths_refer_to_same_location(path, &bundle_project_path));

        if bundle_dir.exists() && !bundle_dir.is_dir() {
            self.status_msg = format!(
                "Bundle error: destination exists and is not a folder: {}",
                bundle_dir.display()
            );
            return false;
        }

        if bundle_dir.exists() && !is_current_bundle {
            self.status_msg = format!(
                "Bundle folder already exists: {}. Choose another destination or rename the project.",
                bundle_dir.display()
            );
            return false;
        }

        self.status_msg = format!("Creating project bundle: {}", bundle_dir.display());
        if let Err(error) = std::fs::create_dir_all(&audio_dir) {
            self.status_msg = format!("Bundle error: {error}");
            error!("Could not create bundle directory: {error}");
            return false;
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
                            return false;
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
                self.assets.set_project_path(bundle_project_path.clone());
                self.report.project_name = self.project.settings.name.clone();
                self.project_needs_save_as = false;
                self.remember_recent_project(&bundle_project_path);
                self.status_msg = format!(
                    "📦 Bundle saved: {} | audio files: {} | missing: {}",
                    bundle_dir.display(),
                    copied_count,
                    missing_count
                );
                self.create_project_backup("bundle-save");
                self.mark_project_snapshot_saved();
                true
            }
            Err(error) => {
                self.status_msg = format!("Bundle save error: {error}");
                error!("Could not save project bundle: {error}");
                false
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

    fn begin_waveform_generation(&mut self) {
        self.waveform_generation = self.waveform_manager.begin_generation();
        self.waveform_pending_cues.clear();
        self.waveform_scheduled_cues.clear();
        self.waveform_total_cues = 0;
        self.waveform_cache_stats_before = audio_analysis_cache_stats();
    }

    fn queue_waveform_analysis_jobs(&mut self, jobs: Vec<(PathBuf, Vec<Uuid>)>) {
        for (path, mut cue_ids) in jobs {
            cue_ids.sort_unstable();
            cue_ids.dedup();
            cue_ids.retain(|cue_id| !self.waveform_scheduled_cues.contains(cue_id));
            if cue_ids.is_empty() {
                continue;
            }

            for cue_id in &cue_ids {
                self.waveform_scheduled_cues.insert(*cue_id);
                self.waveform_pending_cues.insert(*cue_id);
            }
            self.waveform_total_cues = self.waveform_total_cues.saturating_add(cue_ids.len());

            if !self.waveform_manager.submit(
                self.waveform_generation,
                path.clone(),
                cue_ids.clone(),
                WAVEFORM_BUCKETS,
            ) {
                self.waveform_total_cues = self.waveform_total_cues.saturating_sub(cue_ids.len());
                for cue_id in cue_ids {
                    self.waveform_pending_cues.remove(&cue_id);
                    self.waveform_scheduled_cues.remove(&cue_id);
                }
                warn!(
                    "Could not queue background waveform analysis for {:?}",
                    path
                );
            }
        }
    }

    fn poll_waveform_analysis(&mut self, ctx: &Context) {
        while let Some(result) = self.waveform_manager.try_recv() {
            if result.generation != self.waveform_generation {
                continue;
            }

            let mut applied_cues = 0_usize;
            for cue_id in result.cue_ids {
                self.waveform_pending_cues.remove(&cue_id);

                let path_matches = self.cue_list.find(cue_id).is_some_and(|cue| {
                    cue.file_path.is_file()
                        && paths_refer_to_same_location(&cue.file_path, &result.path)
                });
                if !path_matches {
                    // The operator may have replaced this Cue's media while the
                    // previous analysis was still running. Let the current path
                    // be queued by the periodic media scan instead.
                    self.waveform_scheduled_cues.remove(&cue_id);
                    continue;
                }

                let audio = &self.audio;
                if let Some(cue) = self.cue_list.find_mut(cue_id) {
                    if !matches!(&cue.cue_type, CueType::Audio) {
                        continue;
                    }
                    if result.analysis.waveform.is_empty()
                        || result.analysis.duration_secs <= 0.0
                        || result.analysis.channels == 0
                    {
                        warn!(
                            "Background waveform analysis returned no usable data: cue_id={}, path='{}'",
                            cue_id,
                            result.path.display()
                        );
                        continue;
                    }

                    cue.waveform = result.analysis.waveform.clone();
                    cue.duration_secs = result.analysis.duration_secs;
                    cue.audio_channels = result.analysis.channels;
                    if cue.loop_region.end_secs <= 0.0 {
                        cue.loop_region.end_secs = cue.duration_secs;
                    } else {
                        cue.loop_region.end_secs = cue.loop_region.end_secs.min(cue.duration_secs);
                    }
                    cue.loop_region.start_secs =
                        cue.loop_region.start_secs.min(cue.loop_region.end_secs);
                    cue.start_secs = cue.start_secs.min(cue.duration_secs);
                    if let Some(end_secs) = cue.end_secs.as_mut() {
                        *end_secs = (*end_secs).min(cue.duration_secs);
                    }
                    ensure_audio_cue_routes(audio, cue);
                    applied_cues += 1;
                }
            }

            if applied_cues > 0 {
                info!(
                    "Background waveform analysis applied: path='{}', cues={}, duration={:.3}s, channels={}",
                    result.path.display(),
                    applied_cues,
                    result.analysis.duration_secs,
                    result.analysis.channels
                );
            }
            ctx.request_repaint();
        }

        if self.waveform_pending_cues.is_empty() && self.waveform_total_cues > 0 {
            let after = audio_analysis_cache_stats();
            info!(
                "Background waveform generation completed: cues={}, memory_hits={}, memory_misses={}, decodes={}, disk_hits={}, disk_misses={}, disk_writes={}, cache_entries={}, evictions={}",
                self.waveform_total_cues,
                after.hits.saturating_sub(self.waveform_cache_stats_before.hits),
                after.misses.saturating_sub(self.waveform_cache_stats_before.misses),
                after.decodes.saturating_sub(self.waveform_cache_stats_before.decodes),
                after.disk_hits.saturating_sub(self.waveform_cache_stats_before.disk_hits),
                after.disk_misses.saturating_sub(self.waveform_cache_stats_before.disk_misses),
                after.disk_writes.saturating_sub(self.waveform_cache_stats_before.disk_writes),
                after.entries,
                after.evictions.saturating_sub(self.waveform_cache_stats_before.evictions),
            );
            self.waveform_total_cues = 0;
        }

        if !self.waveform_pending_cues.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn poll_audio_file_metadata(&mut self) {
        if self.last_media_scan_at.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_media_scan_at = Instant::now();

        let mut missing_cues = Vec::new();
        let mut jobs: HashMap<PathBuf, (PathBuf, Vec<Uuid>)> = HashMap::new();

        for cue in &self.cue_list.cues {
            if !matches!(&cue.cue_type, CueType::Audio) {
                continue;
            }

            if !cue.file_path.is_file() {
                missing_cues.push(cue.id);
                continue;
            }

            if cue.duration_secs > 0.0 && cue.waveform.len() >= WAVEFORM_BUCKETS.saturating_mul(2) {
                continue;
            }
            if self.waveform_scheduled_cues.contains(&cue.id) {
                continue;
            }

            let key = waveform_path_key(&cue.file_path);
            jobs.entry(key)
                .or_insert_with(|| (cue.file_path.clone(), Vec::new()))
                .1
                .push(cue.id);
        }

        for cue_id in missing_cues {
            if let Some(cue) = self.cue_list.find_mut(cue_id) {
                cue.duration_secs = 0.0;
                cue.waveform = std::sync::Arc::from(Vec::<f32>::new());
            }
            self.waveform_pending_cues.remove(&cue_id);
            self.waveform_scheduled_cues.remove(&cue_id);
        }

        self.queue_waveform_analysis_jobs(
            jobs.into_values()
                .map(|(path, cue_ids)| (path, cue_ids))
                .collect(),
        );
    }

    fn set_audio_cue_file(&mut self, cue_id: Uuid, source_path: PathBuf) {
        if !source_path.is_file() {
            self.status_msg = format!("Audio file does not exist: {}", source_path.display());
            return;
        }

        let analysis = analyze_audio_file(&source_path, WAVEFORM_BUCKETS);
        let duration = analysis.duration_secs;
        let waveform = analysis.waveform.clone();
        let channels = analysis.channels;
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
                let route_backend = cue
                    .io
                    .output_backend
                    .or_else(|| {
                        cue.io
                            .output_device_id
                            .as_deref()
                            .and_then(|id| self.audio.device_registry.find_by_id(id))
                            .map(|endpoint| endpoint.backend)
                    })
                    .unwrap_or(self.audio.backend);
                let route_device_name = cue
                    .io
                    .output_device
                    .clone()
                    .or_else(|| {
                        cue.io
                            .output_device_id
                            .as_deref()
                            .and_then(|id| self.audio.device_registry.find_by_id(id))
                            .filter(|endpoint| endpoint.backend == route_backend)
                            .map(|endpoint| endpoint.device_name.clone())
                    })
                    .unwrap_or_else(|| self.audio.current_device.clone());
                let registered_route_id =
                    if cue.io.output_device_id.is_none() && cue.io.output_device.is_some() {
                        self.audio
                            .registered_endpoint_id(route_backend, &route_device_name)
                    } else {
                        None
                    };
                let route_endpoint_id = cue
                    .io
                    .output_device_id
                    .as_deref()
                    .or(registered_route_id.as_deref())
                    .or_else(|| {
                        cue.io
                            .output_device
                            .is_none()
                            .then_some(self.audio.current_device_id.as_str())
                    });
                let output_count = self.audio.output_channels_for_endpoint(
                    route_backend,
                    route_endpoint_id,
                    Some(&route_device_name),
                );
                let default_route_channels = self.audio.default_route_channels_for_source_endpoint(
                    route_backend,
                    route_endpoint_id,
                    Some(&route_device_name),
                    channels,
                );
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
        let file = FileDialog::new()
            .add_filter("Theatre Show Cue Project", &["tsq", "clabs"])
            .pick_file();

        if let Some(path) = file {
            self.open_project_path(path);
        }
    }

    fn open_project_path(&mut self, path: PathBuf) {
        if !path.is_file() {
            self.status_msg = format!("Project file is unavailable: {}", path.display());
            warn!(
                "Project could not be opened because the file is missing: {}",
                path.display()
            );
            return;
        }

        self.stop_all();
        match Project::load(&path) {
            Ok(mut proj) => {
                let mut cl = proj.cue_list.clone();
                let migration_output_orders = proj
                    .audio_routing
                    .as_ref()
                    .map(|routing| routing.output_channel_orders.clone())
                    .unwrap_or_else(|| self.settings.output_channel_orders.clone());
                if proj.version < 3 {
                    // Generic conversion of legacy native-index routing to
                    // logical TaSQ destinations. This occurs only once when
                    // loading an old project, never when changing Mapping.
                    for cue in &mut cl.cues {
                        if !cue_owns_audio_stream(cue) {
                            continue;
                        }
                        let backend = cue
                            .io
                            .output_backend
                            .or_else(|| {
                                cue.io
                                    .output_device_id
                                    .as_deref()
                                    .and_then(|id| self.audio.device_registry.find_by_id(id))
                                    .map(|endpoint| endpoint.backend)
                            })
                            .unwrap_or(self.audio.backend);
                        let device_name = cue
                            .io
                            .output_device
                            .as_deref()
                            .or_else(|| {
                                cue.io
                                    .output_device_id
                                    .as_deref()
                                    .and_then(|id| self.audio.device_registry.find_by_id(id))
                                    .filter(|endpoint| endpoint.backend == backend)
                                    .map(|endpoint| endpoint.device_name.as_str())
                            })
                            .unwrap_or(self.audio.current_device.as_str());
                        let device_key = format!("{}::{}", backend.label(), device_name);
                        let registered_endpoint_id = if cue.io.output_device_id.is_none() {
                            self.audio.registered_endpoint_id(backend, device_name)
                        } else {
                            None
                        };
                        let endpoint_id = cue
                            .io
                            .output_device_id
                            .as_deref()
                            .or(registered_endpoint_id.as_deref());
                        let output_count = self.audio.output_channels_for_endpoint(
                            backend,
                            endpoint_id,
                            Some(device_name),
                        );
                        let order: Vec<usize> = migration_output_orders
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

                // Projects created before version 4 did not carry their own
                // Logical Output Mapping / Default Stereo Pair. Migrate them
                // once from the current workstation defaults, then persist
                // the resulting profile on the next project save.
                let project_audio_routing = proj.audio_routing.clone().unwrap_or_else(|| {
                    info!(
                        "Project '{}' has no project-scoped audio routing; importing workstation defaults",
                        proj.settings.name
                    );
                    ProjectAudioRouting::from_maps(
                        self.settings.output_channel_orders.clone(),
                        self.settings.default_output_pairs.clone(),
                    )
                });
                proj.audio_routing = Some(project_audio_routing.clone());
                proj.version = proj.version.max(4);
                self.audio
                    .set_output_channel_orders(project_audio_routing.output_channel_orders.clone());
                self.audio
                    .set_default_output_pairs(project_audio_routing.default_output_pairs.clone());

                // Waveform and duration analysis is intentionally deferred to
                // background workers. The Cue List can be installed immediately;
                // each unique media path is analysed once and shared by its Cues.
                let mut waveform_jobs: HashMap<PathBuf, (PathBuf, Vec<Uuid>)> = HashMap::new();
                let mut audio_cue_count = 0_usize;

                for cue in &mut cl.cues {
                    if !matches!(&cue.cue_type, CueType::Audio)
                        || cue.file_path.as_os_str().is_empty()
                    {
                        continue;
                    }

                    audio_cue_count += 1;
                    ensure_audio_cue_routes(&self.audio, cue);

                    if !cue.file_path.is_file() {
                        warn!(
                            "Audio media is missing while opening project: cue_id={}, path='{}'",
                            cue.id,
                            cue.file_path.display()
                        );
                        continue;
                    }

                    let key = waveform_path_key(&cue.file_path);
                    waveform_jobs
                        .entry(key)
                        .or_insert_with(|| (cue.file_path.clone(), Vec::new()))
                        .1
                        .push(cue.id);
                }

                let waveform_job_count = waveform_jobs.len();
                let waveform_cue_count: usize = waveform_jobs
                    .values()
                    .map(|(_, cue_ids)| cue_ids.len())
                    .sum();

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
                    self.remember_recent_project(&path);
                } else {
                    // Opening a backup is a recovery operation. Keep the
                    // snapshot untouched and let the user Save As a normal
                    // project when ready.
                    self.assets = AssetManager::new();
                    self.project_path = None;
                }
                self.project_needs_save_as = false;
                self.begin_waveform_generation();
                self.queue_waveform_analysis_jobs(
                    waveform_jobs
                        .into_values()
                        .map(|(path, cue_ids)| (path, cue_ids))
                        .collect(),
                );
                self.mark_project_snapshot_saved();
                self.last_backup_at = Instant::now();
                self.last_media_scan_at = Instant::now();
                self.report = SessionReport::new(&self.project.settings.name);
                self.selected = None;
                self.selected_ids.clear();
                self.selection_anchor = None;
                info!(
                    "Project opened: path='{}', cues={}, audio_cues={}, waveform_jobs={}, queued_waveform_cues={}; analysis continues in background",
                    path.display(),
                    self.cue_list.cues.len(),
                    audio_cue_count,
                    waveform_job_count,
                    waveform_cue_count
                );
                self.status_msg = format!("📂 Opened: {}", path.display());
            }
            Err(e) => {
                self.status_msg = format!("Open error: {e}");
                error!("{e}");
            }
        }
    }

    fn new_project(&mut self) {
        info!("New project requested");
        self.stop_all();
        self.begin_waveform_generation();
        self.cue_list = CueList::new("Main");
        self.cue_clipboard.clear();
        self.time_trigger_last_fired.clear();
        let audio_routing = self.current_project_audio_routing();
        self.project = Project::with_audio_routing(audio_routing);
        self.apply_project_audio_routing();
        self.project_path = None;
        self.project_needs_save_as = false;
        self.assets = AssetManager::new();
        self.mark_project_snapshot_saved();
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

        if !finished_cues.is_empty() {
            self.begin_launch_transaction();
            for cue_id in finished_cues {
                self.schedule_cue_completion(cue_id, 0.0);
            }
            self.end_launch_transaction();
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
        self.midi_out.poll();
        let messages = self.midi.drain();
        if messages.is_empty() {
            return;
        }
        self.begin_launch_transaction();
        for msg in messages {
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
        self.end_launch_transaction();
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
            tracing::info!(
                target: "TaSQ::osc::server",
                "OSC control dispatched: address='{}', args={:?}",
                &ev.addr,
                &ev.args
            );
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
        // The settings window is modal. Keep its explicit global hotkey capture
        // active, but never dispatch Cue or application shortcuts behind it.
        if self.settings_open {
            self.cue_hotkey_capture = None;
            if self.hotkey_capture.is_none() {
                return;
            }
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

        // Cue-specific capture has priority over all normal shortcuts.
        if let Some(cue_id) = self.cue_hotkey_capture {
            if self.active_panel != Panel::Inspector || self.selected != Some(cue_id) {
                self.cue_hotkey_capture = None;
            } else {
                if let Some((key, physical_key, modifiers)) = key_events.first().copied() {
                    ctx.input_mut(|input| {
                        input.consume_key(modifiers, key);
                    });
                    self.cue_hotkey_capture = None;
                    let capture_key = physical_key.unwrap_or(key);
                    if capture_key == Key::Escape {
                        self.status_msg = "Cue hotkey assignment cancelled".to_string();
                    } else if capture_key == Key::Backspace {
                        if let Some(cue) = self.cue_list.find_mut(cue_id) {
                            cue.hotkey = None;
                            self.status_msg = format!("Hotkey cleared for Cue {}", cue.number);
                            info!("Cue hotkey cleared: id={}, number='{}'", cue.id, cue.number);
                        }
                    } else {
                        let binding = Self::format_hotkey(capture_key, modifiers);
                        let mut global_conflict = None;
                        for action in AppAction::ALL {
                            if self
                                .settings
                                .hotkeys
                                .bindings
                                .get(action)
                                .is_some_and(|existing| existing.eq_ignore_ascii_case(&binding))
                            {
                                global_conflict = Some(action.label().to_string());
                                break;
                            }
                        }
                        let reserved_shortcut =
                            ["Ctrl+Shift+S", "Ctrl+Alt+S", "Ctrl+C", "Ctrl+V", "Ctrl+A"]
                                .iter()
                                .any(|reserved| reserved.eq_ignore_ascii_case(&binding));
                        let cue_conflict = self
                            .cue_list
                            .cues
                            .iter()
                            .find(|cue| {
                                cue.id != cue_id
                                    && cue.hotkey.as_deref().is_some_and(|existing| {
                                        existing.eq_ignore_ascii_case(&binding)
                                    })
                            })
                            .map(|cue| format!("#{} — {}", cue.number, cue.name));

                        if reserved_shortcut {
                            self.status_msg = format!(
                                "{binding} is reserved for a built-in application shortcut"
                            );
                        } else if let Some(action_label) = global_conflict {
                            self.status_msg = format!(
                                "{binding} is already assigned to the global command {action_label}"
                            );
                        } else if let Some(other_cue) = cue_conflict {
                            self.status_msg =
                                format!("{binding} is already assigned to Cue {other_cue}");
                        } else if let Some(cue) = self.cue_list.find_mut(cue_id) {
                            cue.hotkey = Some(binding.clone());
                            self.status_msg =
                                format!("Hotkey {binding} assigned to Cue {}", cue.number);
                            info!(
                                "Cue hotkey assigned: id={}, number='{}', name='{}', binding='{}'",
                                cue.id, cue.number, cue.name, binding
                            );
                        }
                    }
                }
                return;
            }
        }

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
                return;
            }
        }

        // Cue hotkeys trigger their Cue directly without changing the operator's
        // current selection. Text editors retain priority through wants_keyboard_input().
        let cue_hotkey_match = self.cue_list.cues.iter().find_map(|cue| {
            let binding = cue.hotkey.as_deref()?;
            let matched = key_events.iter().any(|(key, physical, modifiers)| {
                Self::format_hotkey(*key, *modifiers).eq_ignore_ascii_case(binding)
                    || physical
                        .as_ref()
                        .map(|physical_key| {
                            Self::format_hotkey(*physical_key, *modifiers)
                                .eq_ignore_ascii_case(binding)
                        })
                        .unwrap_or(false)
            });
            matched.then_some((cue.id, cue.number.clone(), cue.name.clone()))
        });
        if let Some((cue_id, number, name)) = cue_hotkey_match {
            info!(
                "Cue hotkey triggered: id={}, number='{}', name='{}'",
                cue_id, number, name
            );
            self.trigger_cue(cue_id);
            return;
        }

        // Keep the built-in group-navigation keys available unless a Cue has
        // explicitly claimed that key combination as its hotkey.
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
            Key::Num0 => "0".to_string(),
            Key::Num1 => "1".to_string(),
            Key::Num2 => "2".to_string(),
            Key::Num3 => "3".to_string(),
            Key::Num4 => "4".to_string(),
            Key::Num5 => "5".to_string(),
            Key::Num6 => "6".to_string(),
            Key::Num7 => "7".to_string(),
            Key::Num8 => "8".to_string(),
            Key::Num9 => "9".to_string(),
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
            AppAction::SaveProject => {
                self.save_project();
            }
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
        let performance_key =
            AudioEngine::output_device_settings_key_for_backend(backend, device_name);
        let legacy_performance = AudioPerformanceSettings {
            sample_rate: self.settings.audio_sample_rate,
            buffer_size_frames: self.settings.audio_buffer_size_frames,
        };
        let mut performance = AudioPerformanceSettings::for_backend(
            backend,
            self.settings
                .audio_performance_settings_by_device
                .get(&performance_key)
                .copied()
                .unwrap_or_else(|| {
                    if same_endpoint {
                        legacy_performance
                    } else {
                        AudioPerformanceSettings::default()
                    }
                }),
        );
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
                                performance = AudioPerformanceSettings::default();
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
            replacement.set_endpoint_preferences(
                self.settings.audio_performance_settings_by_device.clone(),
                self.settings.output_channel_settings_by_device.clone(),
            );
            let project_audio_routing = self.current_project_audio_routing();
            replacement
                .set_default_output_pairs(project_audio_routing.default_output_pairs.clone());
            replacement
                .set_output_channel_orders(project_audio_routing.output_channel_orders.clone());

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
            // The manager restarts only this endpoint worker; other devices
            // must remain untouched during manual recovery.
            if let Err(error) = self.audio.reconfigure_output(performance) {
                self.status_msg = format!(
                    "Could not apply audio performance settings to {} — {}: {}",
                    backend.short_label(),
                    device_name,
                    error
                );
                warn!("{}", self.status_msg);
                self.active_output_needs_reopen = true;
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
                performance = AudioPerformanceSettings::default();
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
        let active_performance_key =
            AudioEngine::output_device_settings_key_for_backend(backend, &active_device_name);
        self.settings
            .audio_performance_settings_by_device
            .insert(active_performance_key, performance);
        self.settings.audio_sample_rate = performance.sample_rate;
        self.settings.audio_buffer_size_frames = performance.buffer_size_frames;
        self.settings.output_channels = self.audio.output_channel_settings();

        let selected_key = self.audio.output_device_settings_key(&active_device_name);
        self.settings
            .output_channel_settings_by_device
            .insert(selected_key, self.settings.output_channels.clone());
        self.audio.set_endpoint_preferences(
            self.settings.audio_performance_settings_by_device.clone(),
            self.settings.output_channel_settings_by_device.clone(),
        );

        // Keep Cue destinations logical and endpoint-specific. Switching the
        // global device must not rewrite routes belonging to other workers.
        for cue in &mut self.cue_list.cues {
            if !matches!(&cue.cue_type, CueType::Audio) || cue.audio_channels == 0 {
                continue;
            }
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
            let route_device_name = cue
                .io
                .output_device
                .clone()
                .or_else(|| {
                    cue.io
                        .output_device_id
                        .as_deref()
                        .and_then(|id| self.audio.device_registry.find_by_id(id))
                        .filter(|endpoint| endpoint.backend == cue_backend)
                        .map(|endpoint| endpoint.device_name.clone())
                })
                .unwrap_or_else(|| self.audio.current_device.clone());
            let registered_route_id =
                if cue.io.output_device_id.is_none() && cue.io.output_device.is_some() {
                    self.audio
                        .registered_endpoint_id(cue_backend, &route_device_name)
                } else {
                    None
                };
            let route_endpoint_id = cue
                .io
                .output_device_id
                .as_deref()
                .or(registered_route_id.as_deref())
                .or_else(|| {
                    cue.io
                        .output_device
                        .is_none()
                        .then_some(self.audio.current_device_id.as_str())
                });
            let output_count = self.audio.output_channels_for_endpoint(
                cue_backend,
                route_endpoint_id,
                Some(&route_device_name),
            );
            let default_route_channels = self.audio.default_route_channels_for_source_endpoint(
                cue_backend,
                route_endpoint_id,
                Some(&route_device_name),
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

    fn remember_recent_project(&mut self, path: &Path) {
        remember_recent_project_path(&mut self.settings.recent_projects, path);
        self.save_app_settings();
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
                                .size(Theme::scaled_font_size(11.0)),
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
                            .size(Theme::scaled_font_size(10.5)),
                        );

                        ui.separator();
                        ui.label("Text & Cue List size");
                        ui.horizontal_wrapped(|ui| {
                            ui.label("Scale:");
                            for preset in UiScalePreset::ALL {
                                if ui
                                    .selectable_value(
                                        &mut self.settings.appearance.ui_scale,
                                        *preset,
                                        preset.label(),
                                    )
                                    .changed()
                                {
                                    appearance_changed = true;
                                }
                            }
                        });
                        if self.settings.appearance.ui_scale == UiScalePreset::Custom {
                            ui.horizontal(|ui| {
                                ui.label("Custom scale:");
                                if ui
                                    .add(
                                        egui::Slider::new(
                                            &mut self.settings.appearance.custom_ui_scale_percent,
                                            75..=150,
                                        )
                                        .suffix("%")
                                        .show_value(true),
                                    )
                                    .changed()
                                {
                                    appearance_changed = true;
                                }
                            });
                        }
                        ui.label(
                            RichText::new(
                                "Changes text and Cue List event row height only. Custom scale range: 75–150%. Toolbar icons, buttons, faders and control dimensions stay unchanged.",
                            )
                            .color(Theme::text_dim())
                            .size(Theme::scaled_font_size(10.5)),
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
                                "Standard button color",
                                &mut self.settings.appearance.button_color,
                            );
                            appearance_changed |= edit_rgb_color(
                                ui,
                                "Upper toolbar button background",
                                &mut self.settings.appearance.toolbar_button_color,
                            );
                            appearance_changed |= edit_rgb_color(
                                ui,
                                "Group outline color",
                                &mut self.settings.appearance.group_outline_color,
                            );
                            appearance_changed |= edit_rgb_color(
                                ui,
                                "Editable fields and dropdown background",
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
                                RichText::new("Editable fields include text boxes, dB/time values, DragValue controls and dropdown selectors. Their text automatically switches to black or white for contrast with the selected background. Toolbar SVG glyphs do the same against their button background.")
                                    .color(Theme::text_dim())
                                    .size(Theme::scaled_font_size(11.0)),
                            );
                            ui.separator();
                            ui.label("Custom toolbar icons");
                            ui.label(
                                RichText::new(
                                    "Choose any supplied SVG icon for each action, or load your own PNG, JPG or WebP image. Built-in SVG glyphs are tinted for contrast against the selected toolbar button background; imported image files keep their original colors.",
                                )
                                .color(Theme::text_dim())
                                .size(Theme::scaled_font_size(10.5)),
                            );

                            for (role, role_label) in TOOLBAR_ICON_ROLES {
                                let current = self
                                    .settings
                                    .appearance
                                    .icon_overrides
                                    .get(*role)
                                    .cloned();
                                let mut selected = current.clone();
                                let current_label = match &current {
                                    None => "Theme default".to_string(),
                                    Some(IconOverride::Builtin(key)) => BUILTIN_ICON_KEYS
                                        .iter()
                                        .find(|(known, _)| *known == key.as_str())
                                        .map(|(_, label)| (*label).to_string())
                                        .unwrap_or_else(|| "Theme default".to_string()),
                                    Some(IconOverride::CustomFile(path)) => format!(
                                        "Custom: {}",
                                        Path::new(path)
                                            .file_name()
                                            .and_then(|name| name.to_str())
                                            .unwrap_or("image")
                                    ),
                                };
                                let mut choose_file = false;
                                let mut reset_icon = false;
                                ui.horizontal(|ui| {
                                    ui.add_sized([150.0, 22.0], egui::Label::new(*role_label));
                                    Theme::input_scope(ui, |ui| {
                                        egui::ComboBox::from_id_salt(format!("custom_icon_choice_{role}"))
                                        .width(150.0)
                                        .selected_text(current_label)
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut selected,
                                                None,
                                                "Theme default",
                                            );
                                            for (builtin_key, builtin_label) in BUILTIN_ICON_KEYS {
                                                ui.selectable_value(
                                                    &mut selected,
                                                    Some(IconOverride::Builtin((*builtin_key).to_string())),
                                                    *builtin_label,
                                                );
                                            }
                                        });
                                    });
                                    if ui.small_button("Choose file…").clicked() {
                                        choose_file = true;
                                    }
                                    if current.is_some() && ui.small_button("Reset").clicked() {
                                        reset_icon = true;
                                    }
                                });

                                if reset_icon {
                                    self.settings.appearance.icon_overrides.remove(*role);
                                    appearance_changed = true;
                                } else if selected != current {
                                    match selected {
                                        Some(choice) => {
                                            self.settings
                                                .appearance
                                                .icon_overrides
                                                .insert((*role).to_string(), choice);
                                        }
                                        None => {
                                            self.settings.appearance.icon_overrides.remove(*role);
                                        }
                                    }
                                    appearance_changed = true;
                                }

                                if choose_file {
                                    if let Some(source) = FileDialog::new()
                                        .add_filter("Image files", &["png", "jpg", "jpeg", "webp"])
                                        .pick_file()
                                    {
                                        match import_custom_icon(&source, role) {
                                            Ok(stored_path) => {
                                                self.settings.appearance.icon_overrides.insert(
                                                    (*role).to_string(),
                                                    IconOverride::CustomFile(stored_path),
                                                );
                                                appearance_changed = true;
                                            }
                                            Err(error) => {
                                                self.status_msg = format!("Custom icon error: {error}");
                                                warn!("{}", self.status_msg);
                                            }
                                        }
                                    }
                                }
                            }
                        } else {
                            ui.add_space(4.0);
                            let description = match self.settings.appearance.preset {
                                ThemePreset::Dark => "Balanced dark interface with soft gray text.",
                                ThemePreset::Light => "Light interface with dark, high-contrast text.",
                                ThemePreset::Black => "Near-black interface with bright text for dark rooms and OLED displays.",
                                ThemePreset::Custom => "Custom color palette.",
                            };
                            ui.label(RichText::new(description).color(Theme::text_dim()).size(Theme::scaled_font_size(11.0)));
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
                            .size(Theme::scaled_font_size(11.0)),
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
                            Theme::input_scope(ui, |ui| {
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
                            .size(Theme::scaled_font_size(10.5)),
                        );
                        for error in self.audio.device_registry.last_errors() {
                            ui.colored_label(egui::Color32::YELLOW, error);
                        }
                        ui.label(
                            RichText::new(
                                "Windows endpoint notifications trigger immediate WASAPI/DirectSound refreshes; a 60-second fallback scan catches missed notifications. ASIO inventory is not re-enumerated while streams may be active: active ASIO streams are monitored through native error callbacks and callback-progress checks. Restart TaSQ after installing/removing ASIO drivers.",
                            )
                            .color(Theme::text_dim())
                            .size(Theme::scaled_font_size(10.0)),
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
                                "WASAPI and DirectSound preferences are saved separately for each endpoint. ASIO always follows the rate and hardware block size currently selected by its driver control panel.",
                            )
                            .color(Theme::text_dim())
                            .size(Theme::scaled_font_size(10.5)),
                        );

                        if self.audio.backend == AudioBackend::Asio {
                            ui.horizontal(|ui| {
                                ui.label("Sample rate");
                                ui.label(
                                    RichText::new(format!(
                                        "Driver-controlled · {} Hz",
                                        self.audio.current_sample_rate
                                    ))
                                    .strong()
                                    .color(Theme::text()),
                                );
                            });
                            ui.horizontal(|ui| {
                                ui.label("Buffer size");
                                ui.label(
                                    RichText::new("Driver-controlled")
                                        .strong()
                                        .color(Theme::text()),
                                );
                            });
                            ui.label(
                                RichText::new(
                                    "ASIO clock and hardware block size are owned by the sound-card driver. TaSQ opens the stream using the current driver settings and does not request an override. Change them in the device's native ASIO control panel, then reopen the endpoint if required.",
                                )
                                .color(Theme::text_dim())
                                .size(Theme::scaled_font_size(10.5)),
                            );
                        } else {
                            let performance_settings_key =
                                AudioEngine::output_device_settings_key_for_backend(
                                    self.audio.backend,
                                    &output,
                                );
                            let previous_performance = self
                                .settings
                                .audio_performance_settings_by_device
                                .get(&performance_settings_key)
                                .copied()
                                .unwrap_or(self.audio.performance_settings);
                            let previous_sample_rate = previous_performance.sample_rate;
                            let previous_buffer_size = previous_performance.buffer_size_frames;
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
                                Theme::input_scope(ui, |ui| {
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
                                Theme::input_scope(ui, |ui| {
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
                            });

                            ui.label(
                                RichText::new(match self.audio.backend {
                                    AudioBackend::Asio => "ASIO: the driver owns the hardware block size. TaSQ shows only sizes reported by the driver; use its control panel for other values.",
                                    AudioBackend::Wasapi => "WASAPI Shared: this requests the shared ring-buffer capacity. Windows still controls the callback period, so actual timing and latency may differ.",
                                    AudioBackend::DirectSound => "DirectSound: this controls TaSQ's queued-audio safety window. Larger queues improve stability but increase latency.",
                                })
                                .color(Theme::text_dim())
                                .size(Theme::scaled_font_size(10.0)),
                            );

                            if selected_sample_rate != previous_sample_rate
                                || selected_buffer_size != previous_buffer_size
                            {
                                let requested_performance = AudioPerformanceSettings {
                                    sample_rate: selected_sample_rate,
                                    buffer_size_frames: selected_buffer_size,
                                };
                                self.settings
                                    .audio_performance_settings_by_device
                                    .insert(performance_settings_key.clone(), requested_performance);
                                self.settings.audio_sample_rate = selected_sample_rate;
                                self.settings.audio_buffer_size_frames = selected_buffer_size;
                                let backend = self.audio.backend;
                                let device_name = self.audio.current_device.clone();
                                let endpoint_id = self.audio.current_device_id.clone();
                                self.active_output_needs_reopen = true;
                                if self.switch_audio_endpoint(backend, &device_name, &endpoint_id) {
                                    self.save_app_settings();
                                } else {
                                    self.settings
                                        .audio_performance_settings_by_device
                                        .insert(performance_settings_key, previous_performance);
                                    self.settings.audio_sample_rate = previous_sample_rate;
                                    self.settings.audio_buffer_size_frames = previous_buffer_size;
                                    self.save_app_settings();
                                }
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
                                .size(Theme::scaled_font_size(11.0)),
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
                                    Theme::input_scope(ui, |ui| {
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
                            self.store_project_output_channel_order(
                                settings_key,
                                output_order.clone(),
                            );
                            // Re-apply converted native routes to active streams. The
                            // stored logical destinations themselves are unchanged.
                            for cue in &self.cue_list.cues {
                                if cue_owns_audio_stream(cue)
                                    && cue.is_active()
                                    && self.cue_output_backend(cue) == self.audio.backend
                                    && cue_uses_output_endpoint(
                                        cue,
                                        self.audio.backend,
                                        &output,
                                        &self.audio.current_device_id,
                                    )
                                {
                                    let route_backend = self.cue_output_backend(cue);
                                    let route_device_name = cue
                                        .io
                                        .output_device
                                        .clone()
                                        .or_else(|| {
                                            cue.io
                                                .output_device_id
                                                .as_deref()
                                                .and_then(|id| self.audio.device_registry.find_by_id(id))
                                                .filter(|endpoint| endpoint.backend == route_backend)
                                                .map(|endpoint| endpoint.device_name.clone())
                                        })
                                        .unwrap_or_else(|| self.audio.current_device.clone());
                                    let registered_route_id = if cue.io.output_device_id.is_none()
                                        && cue.io.output_device.is_some()
                                    {
                                        self.audio.registered_endpoint_id(
                                            route_backend,
                                            &route_device_name,
                                        )
                                    } else {
                                        None
                                    };
                                    let route_endpoint_id = cue
                                        .io
                                        .output_device_id
                                        .as_deref()
                                        .or(registered_route_id.as_deref())
                                        .or_else(|| {
                                            cue.io.output_device.is_none()
                                                .then_some(self.audio.current_device_id.as_str())
                                        });
                                    self.audio.send(AudioCommand::SetRouting {
                                        id: cue.id,
                                        routes: self.audio.cue_routes_for_endpoint(
                                            route_backend,
                                            route_endpoint_id,
                                            Some(&route_device_name),
                                            &cue.io.channel_routes,
                                        ),
                                    });
                                    self.audio.send(AudioCommand::SetOutputGains {
                                        id: cue.id,
                                        gains_db: self.audio.cue_gains_for_endpoint(
                                            route_backend,
                                            route_endpoint_id,
                                            Some(&route_device_name),
                                            &cue.io.output_gains_db,
                                        ),
                                    });
                                }
                            }
                            info!("Logical output mapping changed for '{}': {:?} -> {:?}; existing Cue assignments and default stereo pair remain unchanged", output, previous_output_order, output_order);
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
                            .size(Theme::scaled_font_size(11.0)),
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
                                        .size(Theme::scaled_font_size(11.0)),
                                    ),
                                );
                                let name_response = Theme::input_scope(ui, |ui| {
                                    ui.add_sized(
                                        [132.0, 26.0],
                                        egui::TextEdit::singleline(&mut output_names[index])
                                            .hint_text("Custom alias"),
                                    )
                                })
                                .inner;
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
                                    .size(Theme::scaled_font_size(11.0)),
                                );
                                if current_pair != detected_logical_pair
                                    && ui.small_button("Use detected pair").clicked()
                                {
                                    self.audio
                                        .set_default_output_pair_for_device(&output, detected_logical_pair);
                                    let settings_key =
                                        self.audio.output_device_settings_key(&output);
                                    self.store_project_default_output_pair(
                                        settings_key,
                                        detected_logical_pair,
                                    );
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
                            self.audio.set_endpoint_preferences(
                                self.settings.audio_performance_settings_by_device.clone(),
                                self.settings.output_channel_settings_by_device.clone(),
                            );
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
                            Theme::input_scope(ui, |ui| {
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
                            });
                            ui.add_sized([18.0, 26.0], egui::Label::new("R"));
                            Theme::input_scope(ui, |ui| {
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
                        });
                        if default_pair_changed {
                            if output_count > 1 && default_pair[0] == default_pair[1] {
                                default_pair[1] = (default_pair[0] + 1) % output_count;
                            }
                            self.audio
                                .set_default_output_pair_for_device(&output, default_pair);
                            let settings_key =
                                self.audio.output_device_settings_key(&output);
                            self.store_project_default_output_pair(settings_key, default_pair);
                        }
                        if default_pair != [0, 1] {
                            ui.colored_label(
                                egui::Color32::YELLOW,
                                "Default Stereo Pair is applied only to newly imported Audio Cues; existing Cue routes are not modified.",
                            );
                        }
                        ui.label(
                            RichText::new(
                                "Logical Output Mapping and Default Stereo Pair are stored with this project. Save the .tsq after changing them; new projects inherit the currently active routing profile.",
                            )
                            .color(Theme::text_dim())
                            .size(Theme::scaled_font_size(10.5)),
                        );

                        let mut input = self.audio.current_input_device.clone();
                        ui.horizontal(|ui| {
                            ui.label("Input device");
                            Theme::input_scope(ui, |ui| {
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
                        });
                        ui.label(RichText::new(
                            "The selected output device and its driver channel count are global for the application.",
                        ).color(Theme::text_dim()).size(Theme::scaled_font_size(11.0)));

                    }
                    SettingsTab::Osc => {
                        ui.heading("Open Sound Control");
                        ui.horizontal(|ui| {
                            ui.label("Listen port");
                            Theme::input_scope(ui, |ui| {
                                ui.add(
                                    egui::DragValue::new(&mut self.settings.osc_listen_port)
                                        .range(1..=65535),
                                );
                            });
                        });
                        ui.horizontal(|ui| {
                            ui.label("Send address");
                            Theme::input_scope(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.settings.osc_send_addr)
                                        .desired_width(280.0),
                                );
                            });
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
                                .size(Theme::scaled_font_size(11.0)),
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
                                                .size(Theme::scaled_font_size(10.5)),
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
                            Theme::input_scope(ui, |ui| {
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
                        });
                        ui.horizontal(|ui| {
                            ui.label("MIDI output");
                            let output_label = self.midi_out.current_port.clone()
                                .unwrap_or_else(|| "No output selected".to_string());
                            Theme::input_scope(ui, |ui| {
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
                            .color(Theme::text_dim()).size(Theme::scaled_font_size(11.0)));
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
                            .size(Theme::scaled_font_size(11.0)),
                        );
                        ui.add_space(8.0);

                        let interval_changed = ui
                            .horizontal(|ui| {
                                ui.label("Automatic backup interval");
                                Theme::input_scope(ui, |ui| {
                                    ui.add(
                                        egui::DragValue::new(
                                            &mut self.settings.backup_interval_minutes,
                                        )
                                        .range(0..=240)
                                        .speed(1)
                                        .suffix(" min"),
                                    )
                                })
                                .inner
                                .changed()
                            })
                            .inner;

                        ui.label(
                            RichText::new("0 disables timed backups; backups after manual saves remain enabled.")
                                .color(Theme::text_dim())
                                .size(Theme::scaled_font_size(11.0)),
                        );
                        ui.label(
                            RichText::new("The backup folder stores event/project data only, not audio media. Theatre Show Cue keeps the 10 newest snapshots and removes the oldest automatically.")
                                .color(Theme::text_dim())
                                .size(Theme::scaled_font_size(11.0)),
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
                            .size(Theme::scaled_font_size(11.0)),
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
                            Theme::input_scope(ui, |ui| {
                                ui.add_sized(
                                    [430.0, 26.0],
                                    egui::TextEdit::singleline(&mut displayed_log_directory)
                                        .interactive(false),
                                );
                            });
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
                            Ok((audio_count, midi_count, osc_count, application_count, report_count)) => {
                                ui.label(
                                    RichText::new(format!(
                                        "Current folder: {audio_count}/30 audio · {midi_count}/30 MIDI · {osc_count}/30 OSC · {application_count}/30 application logs · {report_count}/30 reports"
                                    ))
                                    .color(Theme::text_dim())
                                    .size(Theme::scaled_font_size(11.0)),
                                );
                            }
                            Err(error) => {
                                ui.label(
                                    RichText::new(format!("Could not read log folder: {error}"))
                                        .color(Theme::text_dim())
                                        .size(Theme::scaled_font_size(11.0)),
                                );
                            }
                        }

                        ui.label(
                            RichText::new(
                                "Automatic cleanup keeps the newest 30 files of each type. Only Theatre Show Cue diagnostic files are removed; unrelated files are never deleted. The export contains logs and the diagnostic report, not audio media or project files.",
                            )
                            .color(Theme::text_dim())
                            .size(Theme::scaled_font_size(11.0)),
                        );

                    }
                }
                    });
            });
        self.settings_open = is_open;
        if !is_open {
            self.hotkey_capture = None;
            self.cue_hotkey_capture = None;
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
                                    .size(Theme::scaled_font_size(18.0))
                                    .strong(),
                            );
                            ui.label(
                                RichText::new("WAV · MP3 · FLAC · OGG · AIFF · M4A")
                                    .color(Theme::text_dim())
                                    .size(Theme::scaled_font_size(12.0)),
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
        let recent_projects = self.settings.recent_projects.clone();
        let mut open_recent_path: Option<PathBuf> = None;

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
                ui.menu_button("Open Recent", |ui| {
                    // Keep recent project names on a single line; reveal the full path only on hover.
                    ui.set_min_width(260.0);
                    ui.set_max_width(360.0);

                    if recent_projects.is_empty() {
                        ui.add_enabled(false, egui::Button::new("No recent projects"));
                    } else {
                        for recent_path in &recent_projects {
                            let path = PathBuf::from(recent_path);
                            let label = recent_project_menu_label(&path);
                            if path.is_file() {
                                if ui
                                    .add(egui::Button::new(label).truncate())
                                    .on_hover_text(recent_project_hover_path(&path))
                                    .clicked()
                                {
                                    open_recent_path = Some(path);
                                    ui.close_menu();
                                }
                            } else {
                                ui.add_enabled(
                                    false,
                                    egui::Button::new(
                                        RichText::new(format!("{label}  (missing)"))
                                            .color(Theme::text_dim()),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(format!(
                                    "Project file not found: {}",
                                    recent_project_hover_path(&path)
                                ));
                            }
                        }
                    }
                });
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
                if ui.button("Export session report…").clicked() {
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
                if ui.button("Audio Log").clicked() {
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

        if let Some(path) = open_recent_path {
            self.open_project_path(path);
        }
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

        let tab_icon_context = ui.ctx().clone();
        let tab_icon_appearance = self.settings.appearance.clone();
        let tab_icon_color = Theme::text();
        let tab_inspector_icon = self.icons.texture_id_for_color(
            &tab_icon_context,
            "settings",
            &tab_icon_appearance,
            tab_icon_color,
        );
        let tab_waveform_icon = self.icons.texture_id_for_color(
            &tab_icon_context,
            "audio",
            &tab_icon_appearance,
            tab_icon_color,
        );
        let tab_playback_icon = self.icons.texture_id_for_color(
            &tab_icon_context,
            "play",
            &tab_icon_appearance,
            tab_icon_color,
        );
        let tab_routing_icon = self.icons.texture_id_for_color(
            &tab_icon_context,
            "audio",
            &tab_icon_appearance,
            tab_icon_color,
        );
        let tab_trim_icon = self.icons.texture_id_for_color(
            &tab_icon_context,
            "audio",
            &tab_icon_appearance,
            tab_icon_color,
        );
        let tab_midi_icon = self.icons.texture_id_for_color(
            &tab_icon_context,
            "midi",
            &tab_icon_appearance,
            tab_icon_color,
        );
        let tab_report_icon = self.icons.texture_id_for_color(
            &tab_icon_context,
            "notes",
            &tab_icon_appearance,
            tab_icon_color,
        );

        ui.horizontal(|ui| {
            if icon_selectable_label(
                ui,
                tab_inspector_icon,
                "Inspector",
                self.active_panel == Panel::Inspector,
            )
            .clicked()
            {
                self.active_panel = Panel::Inspector;
            }
            if selected_is_audio {
                if icon_selectable_label(
                    ui,
                    tab_waveform_icon,
                    "Waveform & Region/Loop",
                    self.active_panel == Panel::Waveform,
                )
                .clicked()
                {
                    self.active_panel = Panel::Waveform;
                }
                if icon_selectable_label(
                    ui,
                    tab_playback_icon,
                    "Playback",
                    self.active_panel == Panel::Playback,
                )
                .clicked()
                {
                    self.active_panel = Panel::Playback;
                }
                if icon_selectable_label(
                    ui,
                    tab_routing_icon,
                    "I/O Routing",
                    self.active_panel == Panel::IoRouting,
                )
                .clicked()
                {
                    self.active_panel = Panel::IoRouting;
                }
                if icon_selectable_label(
                    ui,
                    tab_trim_icon,
                    "Trim",
                    self.active_panel == Panel::Trim,
                )
                .clicked()
                {
                    self.active_panel = Panel::Trim;
                }
            }
            if selected_is_midi
                && icon_selectable_label(
                    ui,
                    tab_midi_icon,
                    "MIDI Settings",
                    self.active_panel == Panel::Midi,
                )
                .clicked()
            {
                self.active_panel = Panel::Midi;
            }
            if icon_selectable_label(
                ui,
                tab_report_icon,
                "Audio Log",
                self.active_panel == Panel::Report,
            )
            .clicked()
            {
                self.active_panel = Panel::Report;
            }
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
                            let capturing_hotkey = self.cue_hotkey_capture == Some(id);
                            let inspector_action = self.cue_list.find_mut(id).and_then(|cue| {
                                Inspector::show_inspector(
                                    ui,
                                    cue,
                                    &cue_targets,
                                    &all_cues,
                                    capturing_hotkey,
                                )
                            });
                            match inspector_action {
                                Some(InspectorAction::SetAudioFile(path)) => {
                                    self.cue_hotkey_capture = None;
                                    self.set_audio_cue_file(id, path);
                                }
                                Some(InspectorAction::CaptureCueHotkey) => {
                                    self.cue_hotkey_capture = Some(id);
                                    self.status_msg = "Press a key combination for this Cue; Esc cancels".to_string();
                                }
                                Some(InspectorAction::ClearCueHotkey) => {
                                    self.cue_hotkey_capture = None;
                                    if let Some(cue) = self.cue_list.find_mut(id) {
                                        cue.hotkey = None;
                                        self.status_msg = format!("Hotkey cleared for Cue {}", cue.number);
                                        info!("Cue hotkey cleared: id={}, number='{}'", cue.id, cue.number);
                                    }
                                }
                                None => {}
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
                                    let route_device_name = cue
                                        .io
                                        .output_device
                                        .clone()
                                        .or_else(|| {
                                            cue.io
                                                .output_device_id
                                                .as_deref()
                                                .and_then(|id| self.audio.device_registry.find_by_id(id))
                                                .filter(|endpoint| endpoint.backend == route_backend)
                                                .map(|endpoint| endpoint.device_name.clone())
                                        })
                                        .unwrap_or_else(|| self.audio.current_device.clone());
                                    let registered_route_id = if cue.io.output_device_id.is_none()
                                        && cue.io.output_device.is_some()
                                    {
                                        self.audio.registered_endpoint_id(
                                            route_backend,
                                            &route_device_name,
                                        )
                                    } else {
                                        None
                                    };
                                    let route_endpoint_id = cue
                                        .io
                                        .output_device_id
                                        .clone()
                                        .or(registered_route_id);
                                    (
                                        cue.io.channel_routes.clone(),
                                        cue.io.output_gains_db.clone(),
                                        cue.volume * self.master_vol,
                                        endpoint_changed,
                                        route_backend,
                                        route_endpoint_id,
                                        route_device_name,
                                        cue.name.clone(),
                                    )
                                });
                                if let Some((
                                    routes,
                                    output_gains_db,
                                    volume,
                                    endpoint_changed,
                                    route_backend,
                                    route_endpoint_id,
                                    route_device_name,
                                    cue_name,
                                )) = cue_update
                                {
                                    if endpoint_changed {
                                        self.stop_target_and_descendants(id);
                                        self.status_msg = format!(
                                            "Cue '{}' stopped because its output endpoint changed. Press GO to play through {} — {}.",
                                            cue_name,
                                            route_backend.short_label(),
                                            route_device_name
                                        );
                                    }
                                    self.audio.send(AudioCommand::SetRouting {
                                        id,
                                        routes: self.audio.cue_routes_for_endpoint(
                                            route_backend,
                                            route_endpoint_id.as_deref(),
                                            Some(&route_device_name),
                                            &routes,
                                        ),
                                    });
                                    self.audio.send(AudioCommand::SetOutputGains {
                                        id,
                                        gains_db: self.audio.cue_gains_for_endpoint(
                                            route_backend,
                                            route_endpoint_id.as_deref(),
                                            Some(&route_device_name),
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
                                if let Some(cue) = self.cue_list.find(id).cloned() {
                                    let (route_backend, route_endpoint_id, route_device_name, _) =
                                        self.cue_output_endpoint_context(&cue);
                                    let routes = cue.io.channel_routes.clone();
                                    let volume = cue.volume * self.master_vol;
                                    self.audio.send(AudioCommand::SetRouting {
                                        id,
                                        routes: self.audio.cue_routes_for_endpoint(
                                            route_backend,
                                            route_endpoint_id.as_deref(),
                                            Some(&route_device_name),
                                            &routes,
                                        ),
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
                        self.audio_log_viewer.show(ui);
                    }
                }
            });
    }
}

impl eframe::App for TheatreShowCueApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        let gui_update_started = Instant::now();
        self.cpu_monitor.update();
        // Re-apply our palette during update as well as at startup. This keeps
        // egui's native-window defaults from overriding custom text colors on Windows.
        Theme::apply(ctx, &self.settings.appearance);
        self.request_application_close(ctx);

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
                self.audio.update_device_registry();

                // Stable IDs remain authoritative; refresh only the saved display
                // name/backend metadata when a device driver renames an endpoint.
                for cue in &mut self.cue_list.cues {
                    if let Some(endpoint_id) = cue.io.output_device_id.as_deref() {
                        if let Some(endpoint) = self.audio.device_registry.find_by_id(endpoint_id) {
                            cue.io.output_device = Some(endpoint.device_name.clone());
                            cue.io.output_backend = Some(endpoint.backend);
                        }
                    }
                }

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
                    let message = format!(
                        "Active audio endpoint {} — '{}' was disconnected. Cues on this endpoint will stop; other audio devices continue.",
                        self.audio.backend.short_label(),
                        self.audio.current_device
                    );
                    self.status_msg = message.clone();
                    warn!("{}", message);
                } else if active_endpoint_available && self.active_output_needs_reopen {
                    self.status_msg = format!(
                        "{} — '{}' reconnected. Restoring its audio worker; playback will not restart automatically.",
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

        for event in self.audio.drain_worker_manager_events() {
            match event {
                AudioWorkerManagerEvent::OutputMeter { peak } => {
                    let peak = if peak.is_finite() { peak.max(0.0) } else { 0.0 };
                    // Fast attack, gradual release keeps the meter readable between 10 Hz updates.
                    self.output_meter_peak = peak.max(self.output_meter_peak * 0.82);
                }
                AudioWorkerManagerEvent::EndpointLost {
                    backend,
                    endpoint_id,
                    device_name,
                    is_global,
                    affected_cues,
                } => {
                    if is_global
                        && backend == self.audio.backend
                        && endpoint_id == self.audio.current_device_id
                    {
                        self.active_output_needs_reopen = true;
                    }
                    let message = if affected_cues > 0 {
                        format!(
                            "{} — '{}' disconnected. {} Cue(s) were stopped; routing was preserved. Reconnect the device and press GO to resume.",
                            backend.short_label(),
                            device_name,
                            affected_cues
                        )
                    } else {
                        format!(
                            "{} — '{}' disconnected. Other audio devices continue unaffected.",
                            backend.short_label(),
                            device_name
                        )
                    };
                    info!(
                        "GUI received endpoint-lost event: {} [{}], global={}, affected_cues={}",
                        device_name, endpoint_id, is_global, affected_cues
                    );
                    self.status_msg = message;
                }
                AudioWorkerManagerEvent::EndpointRecovered {
                    metadata,
                    is_global,
                } => {
                    if is_global
                        && metadata.backend == self.audio.backend
                        && metadata.current_device_id == self.audio.current_device_id
                    {
                        self.audio.apply_recovered_worker_metadata(&metadata);
                        self.active_output_needs_reopen = false;
                    }
                    self.status_msg = format!(
                        "{} — '{}' is back online. Affected Cues are stopped and ready; press GO to play.",
                        metadata.backend.short_label(),
                        metadata.current_device
                    );
                    info!(
                        "GUI received endpoint-recovered event: {} [{}], global={}",
                        metadata.current_device, metadata.current_device_id, is_global
                    );
                }
                AudioWorkerManagerEvent::EndpointRecoveryFailed {
                    backend,
                    endpoint_id,
                    device_name,
                    is_global,
                    error,
                    retry_scheduled,
                } => {
                    if is_global
                        && backend == self.audio.backend
                        && endpoint_id == self.audio.current_device_id
                    {
                        self.active_output_needs_reopen = true;
                    }
                    let recovery_status = if retry_scheduled {
                        "TaSQ will continue its scheduled recovery checks and will create a Worker only when the endpoint is available. Affected Cues will not resume by themselves."
                    } else {
                        "Automatic recovery has stopped after the configured attempt limit. Reconnect the device or press GO to start a new recovery cycle."
                    };
                    let recovery_headline = if error.starts_with("Audio Worker health failure:") {
                        "Audio Worker stopped"
                    } else {
                        "Could not restore"
                    };
                    self.status_msg = format!(
                        "{} {} — '{}'. {} {}",
                        recovery_headline,
                        backend.short_label(),
                        device_name,
                        recovery_status,
                        error
                    );
                    warn!(
                        "GUI received endpoint-recovery-failed event: {} [{}], global={}: {}",
                        device_name, endpoint_id, is_global, error
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
        self.poll_waveform_analysis(ctx);
        self.poll_audio_file_metadata();
        self.poll_periodic_backup();
        self.handle_keyboard(ctx);
        self.handle_drop(ctx);

        if !self.waveform_pending_cues.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(50));
        } else if self.cue_list.active_count() > 0 || !self.pending_triggers.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(30));
        } else if self.output_meter_peak > 0.0001 {
            ctx.request_repaint_after(Duration::from_millis(100));
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
                            .size(Theme::scaled_font_size(11.0)),
                    );
                    if !self.waveform_pending_cues.is_empty() {
                        let total = self
                            .waveform_total_cues
                            .max(self.waveform_pending_cues.len());
                        let completed = total.saturating_sub(self.waveform_pending_cues.len());
                        ui.add(
                            egui::ProgressBar::new(completed as f32 / total.max(1) as f32)
                                .desired_width(150.0)
                                .text(format!("Waveforms {completed}/{total}")),
                        );
                    }
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
                            .size(Theme::scaled_font_size(11.0)),
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
                    let total = self.cue_list.cues.len();
                    let output_peak = self.output_meter_peak;
                    let cpu_usage = self.cpu_monitor.usage_percent();
                    let prev_master = self.master_vol;
                    let transport_appearance = self.settings.appearance.clone();
                    let action = TransportBar::show(
                        ui,
                        &mut self.icons,
                        &transport_appearance,
                        active,
                        total,
                        output_peak,
                        cpu_usage,
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
                            Theme::input_scope(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut cue.notes)
                                        .font(egui::FontId::proportional(Theme::scaled_font_size(
                                            16.0,
                                        )))
                                        .desired_width(notes_width)
                                        .desired_rows(12)
                                        .hint_text("Notes for selected event…"),
                                );
                            });
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
                                        let response = Theme::input_scope(ui, |ui| {
                                            ui.add(
                                                egui::TextEdit::singleline(edit_name)
                                                    .desired_width(190.0)
                                                    .font(egui::FontId::proportional(
                                                        Theme::scaled_font_size(14.0),
                                                    )),
                                            )
                                        })
                                        .inner;
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
                                                    .size(Theme::scaled_font_size(14.0)),
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
                                                .size(Theme::scaled_font_size(13.0)),
                                            ),
                                        )
                                        .on_hover_text(name.as_str());
                                        ui.add_sized(
                                            [70.0, 20.0],
                                            egui::Label::new(
                                                egui::RichText::new(cue_type)
                                                    .color(Theme::text_dim())
                                                    .size(Theme::scaled_font_size(11.0)),
                                            ),
                                        );
                                    } else {
                                        ui.add_sized(
                                            [270.0, 20.0],
                                            egui::Label::new(
                                                egui::RichText::new("No event selected")
                                                    .color(Theme::text_dim())
                                                    .size(Theme::scaled_font_size(12.0)),
                                            ),
                                        );
                                        ui.add_sized([70.0, 20.0], egui::Label::new(""));
                                    }
                                });

                                ui.add_space(3.0);
                                let icon_context = ui.ctx().clone();
                                let icon_appearance = self.settings.appearance.clone();
                                let (button_fill, button_icon_color) =
                                    toolbar_icon_colors(&icon_appearance);
                                let button_border = Theme::border();
                                let icon_audio = self
                                    .icons
                                    .texture_id_for_color(
                                        &icon_context,
                                        "audio",
                                        &icon_appearance,
                                        button_icon_color,
                                    )
                                    .expect("embedded audio SVG");
                                let icon_play = self
                                    .icons
                                    .texture_id_for_color(
                                        &icon_context,
                                        "play",
                                        &icon_appearance,
                                        button_icon_color,
                                    )
                                    .expect("embedded play SVG");
                                let icon_pause = self
                                    .icons
                                    .texture_id_for_color(
                                        &icon_context,
                                        "pause",
                                        &icon_appearance,
                                        button_icon_color,
                                    )
                                    .expect("embedded pause SVG");
                                let icon_stop = self
                                    .icons
                                    .texture_id_for_color(
                                        &icon_context,
                                        "stop",
                                        &icon_appearance,
                                        button_icon_color,
                                    )
                                    .expect("embedded stop SVG");
                                let icon_fade = self
                                    .icons
                                    .texture_id_for_color(
                                        &icon_context,
                                        "fade",
                                        &icon_appearance,
                                        button_icon_color,
                                    )
                                    .expect("embedded fade SVG");
                                let icon_midi = self
                                    .icons
                                    .texture_id_for_color(
                                        &icon_context,
                                        "midi",
                                        &icon_appearance,
                                        button_icon_color,
                                    )
                                    .expect("embedded MIDI SVG");
                                let icon_osc = self
                                    .icons
                                    .texture_id_for_color(
                                        &icon_context,
                                        "osc",
                                        &icon_appearance,
                                        button_icon_color,
                                    )
                                    .expect("embedded OSC SVG");
                                let icon_group = self
                                    .icons
                                    .texture_id_for_color(
                                        &icon_context,
                                        "group",
                                        &icon_appearance,
                                        button_icon_color,
                                    )
                                    .expect("embedded group SVG");
                                let icon_comment = self
                                    .icons
                                    .texture_id_for_color(
                                        &icon_context,
                                        "comment",
                                        &icon_appearance,
                                        button_icon_color,
                                    )
                                    .expect("embedded comment SVG");
                                let icon_notes = self
                                    .icons
                                    .texture_id_for_color(
                                        &icon_context,
                                        "notes",
                                        &icon_appearance,
                                        button_icon_color,
                                    )
                                    .expect("embedded notes SVG");
                                let icon_settings = self
                                    .icons
                                    .texture_id_for_color(
                                        &icon_context,
                                        "settings",
                                        &icon_appearance,
                                        button_icon_color,
                                    )
                                    .expect("embedded settings SVG");
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    ui.add_space(8.0);
                                    // 42x42 clickable surface with a 38x38 SVG glyph.
                                    let button_size = egui::vec2(42.0, 42.0);
                                    if icon_button(
                                        ui,
                                        icon_audio,
                                        button_size,
                                        button_fill,
                                        button_border,
                                    )
                                    .on_hover_text("Add audio file(s)")
                                    .clicked()
                                    {
                                        self.add_files();
                                    }
                                    if icon_button(
                                        ui,
                                        icon_play,
                                        button_size,
                                        button_fill,
                                        button_border,
                                    )
                                    .on_hover_text("Add Play cue")
                                    .clicked()
                                    {
                                        self.add_typed_cue(CueType::Play { target_id: None });
                                    }
                                    if icon_button(
                                        ui,
                                        icon_pause,
                                        button_size,
                                        button_fill,
                                        button_border,
                                    )
                                    .on_hover_text("Add Pause cue")
                                    .clicked()
                                    {
                                        self.add_typed_cue(CueType::Pause { target_id: None });
                                    }
                                    if icon_button(
                                        ui,
                                        icon_stop,
                                        button_size,
                                        button_fill,
                                        button_border,
                                    )
                                    .on_hover_text("Add Stop cue")
                                    .clicked()
                                    {
                                        self.add_typed_cue(CueType::Stop { target_id: None });
                                    }
                                    if icon_button(
                                        ui,
                                        icon_fade,
                                        button_size,
                                        button_fill,
                                        button_border,
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
                                    if icon_button(
                                        ui,
                                        icon_midi,
                                        button_size,
                                        button_fill,
                                        button_border,
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
                                    if icon_button(
                                        ui,
                                        icon_osc,
                                        button_size,
                                        button_fill,
                                        button_border,
                                    )
                                    .on_hover_text("Add OSC cue")
                                    .clicked()
                                    {
                                        self.add_typed_cue(CueType::Osc {
                                            address: "/tasq/event".to_string(),
                                            args: Vec::new(),
                                        });
                                    }
                                    if icon_button(
                                        ui,
                                        icon_group,
                                        button_size,
                                        button_fill,
                                        button_border,
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
                                    if icon_button(
                                        ui,
                                        icon_comment,
                                        button_size,
                                        button_fill,
                                        button_border,
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
                                            Theme::input_scope(ui, |ui| {
                                                ui.add_sized(
                                                    [notes_width, 47.0],
                                                    egui::TextEdit::multiline(&mut cue.notes)
                                                        .desired_rows(2)
                                                        .hint_text("Notes for selected event…"),
                                                );
                                            });
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

                                    if icon_button(
                                        ui,
                                        icon_notes,
                                        egui::vec2(42.0, 42.0),
                                        button_fill,
                                        button_border,
                                    )
                                    .on_hover_text("Notes sidebar")
                                    .clicked()
                                    {
                                        self.notes_sidebar_open = !self.notes_sidebar_open;
                                    }
                                    if icon_button(
                                        ui,
                                        icon_settings,
                                        egui::vec2(42.0, 42.0),
                                        button_fill,
                                        button_border,
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
                            let cue_icon_appearance = self.settings.appearance.clone();
                            let cue_icons = &mut self.icons;
                            let action = ui
                                .add_enabled_ui(interactions_enabled, |ui| {
                                    CueTable::show(
                                        ui,
                                        cue_icons,
                                        &cue_icon_appearance,
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
                                        self.commit_cue_edit(id, field, new_val);
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
        self.show_close_confirmation(ctx);

        let gui_update_elapsed = gui_update_started.elapsed();
        if gui_update_elapsed >= Duration::from_millis(33)
            && SLOW_GUI_UPDATE_LOG_COUNTER.fetch_add(1, Ordering::Relaxed) % 30 == 0
        {
            warn!(
                "GUI update exceeded 30 FPS frame budget: {:.2} ms; cues={}, active_cues={}, pending_triggers={}",
                gui_update_elapsed.as_secs_f64() * 1_000.0,
                self.cue_list.cues.len(),
                self.cue_list.active_count(),
                self.pending_triggers.len()
            );
        }
    }
}

#[cfg(test)]
mod normal_group_tests {
    use super::{normal_group_next_manual_cue_in, TheatreShowCueApp};
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
    fn comment_rows_are_skipped_when_selecting_the_next_manual_cue() {
        let first = audio_cue(ContinueMode::AutoFollow);
        let comment = comment_cue();
        let manual = audio_cue(ContinueMode::Nothing);
        let after = audio_cue(ContinueMode::Nothing);
        let after_id = after.id;
        let children = vec![first, comment, manual.clone(), after];
        let group = normal_group(&children);
        let mut cues = children;
        cues.push(group.clone());

        assert_eq!(
            normal_group_next_manual_cue_in(&cues, group.id, &mut HashSet::new()),
            Some((vec![group.id], after_id))
        );
    }

    #[test]
    fn first_nothing_is_started_by_group_go_and_selects_the_second_child() {
        let first = audio_cue(ContinueMode::Nothing);
        let second = audio_cue(ContinueMode::Nothing);
        // This Cue is still manual; its Auto-Continue applies only after it starts.
        let third = audio_cue(ContinueMode::AutoContinue);
        let fourth = audio_cue(ContinueMode::Nothing);
        let children = vec![first, second.clone(), third, fourth];
        let group = normal_group(&children);
        let mut cues = children;
        cues.push(group.clone());

        assert_eq!(
            normal_group_next_manual_cue_in(&cues, group.id, &mut HashSet::new()),
            Some((vec![group.id], second.id))
        );
    }

    #[test]
    fn auto_continue_on_next_cue_does_not_make_that_cue_automatic() {
        let first = audio_cue(ContinueMode::AutoContinue);
        let reached_automatically = audio_cue(ContinueMode::Nothing);
        // This Cue requires manual GO; its Auto-Continue starts the following Cue.
        let next_manual = audio_cue(ContinueMode::AutoContinue);
        let after_auto_continue = audio_cue(ContinueMode::Nothing);
        let children = vec![
            first,
            reached_automatically,
            next_manual.clone(),
            after_auto_continue,
        ];
        let group = normal_group(&children);
        let mut cues = children;
        cues.push(group.clone());

        assert_eq!(
            normal_group_next_manual_cue_in(&cues, group.id, &mut HashSet::new()),
            Some((vec![group.id], next_manual.id))
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
            normal_group_next_manual_cue_in(&cues, group.id, &mut HashSet::new()),
            None
        );
    }

    #[test]
    fn nothing_before_later_children_selects_the_following_manual_cue() {
        let children = vec![
            audio_cue(ContinueMode::AutoContinue),
            audio_cue(ContinueMode::AutoFollow),
            audio_cue(ContinueMode::AutoContinue),
            audio_cue(ContinueMode::Nothing),
            audio_cue(ContinueMode::AutoContinue),
            audio_cue(ContinueMode::Nothing),
        ];
        let group = normal_group(&children);
        let next_manual_id = children[4].id;
        let mut cues = children;
        cues.push(group.clone());

        assert_eq!(
            normal_group_next_manual_cue_in(&cues, group.id, &mut HashSet::new()),
            Some((vec![group.id], next_manual_id))
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
        let next_manual_id = nested_children[2].id;
        let mut cues = nested_children;
        cues.push(nested.clone());
        cues.push(after_nested);
        cues.push(outer.clone());

        assert_eq!(
            normal_group_next_manual_cue_in(&cues, outer.id, &mut HashSet::new()),
            Some((vec![outer.id, nested.id], next_manual_id))
        );
    }
}

#[cfg(test)]
mod completion_post_wait_tests {
    use super::{completion_owns_post_wait, completion_post_wait_delay};
    use crate::cue::ContinueMode;

    #[test]
    fn auto_continue_owns_post_wait_outside_managed_groups() {
        assert!(!completion_owns_post_wait(
            &ContinueMode::AutoContinue,
            false,
            false,
            false,
        ));
    }

    #[test]
    fn playlist_child_completion_owns_post_wait() {
        assert!(completion_owns_post_wait(
            &ContinueMode::AutoContinue,
            true,
            false,
            false,
        ));
    }

    #[test]
    fn all_cues_child_keeps_its_existing_group_completion_path() {
        assert!(completion_owns_post_wait(
            &ContinueMode::AutoContinue,
            false,
            true,
            false,
        ));
    }

    #[test]
    fn managed_playlist_or_all_cues_group_owns_its_final_post_wait() {
        assert!(completion_owns_post_wait(
            &ContinueMode::AutoContinue,
            false,
            false,
            true,
        ));
    }

    #[test]
    fn auto_follow_and_nothing_keep_completion_owned_post_wait() {
        assert!(completion_owns_post_wait(
            &ContinueMode::AutoFollow,
            false,
            false,
            false,
        ));
        assert!(completion_owns_post_wait(
            &ContinueMode::Nothing,
            false,
            false,
            false,
        ));
    }

    #[test]
    fn auto_continue_audio_finishing_after_post_wait_does_not_restart_it() {
        assert_eq!(completion_post_wait_delay(0.0, 2.0, None, false), 0.0);
    }

    #[test]
    fn auto_continue_audio_finishing_during_post_wait_uses_only_remaining_time() {
        assert_eq!(
            completion_post_wait_delay(0.0, 2.0, Some(0.75), false),
            0.75
        );
    }

    #[test]
    fn playlist_still_waits_after_audio_completion() {
        assert_eq!(completion_post_wait_delay(0.0, 2.0, None, true), 2.0);
    }

    #[test]
    fn external_wait_and_fade_completion_wait_for_the_later_deadline() {
        assert_eq!(completion_post_wait_delay(3.0, 2.0, Some(1.0), false), 3.0);
    }
}

#[cfg(test)]
mod all_cues_timing_tests {
    use super::build_all_cues_segments;
    use crate::cue::{Cue, CueList, CueType};

    #[test]
    fn all_cues_splits_at_pre_and_post_wait_boundaries() {
        let mut cue_list = CueList::new("All Cues timing test");
        let mut ids = Vec::new();

        for index in 0..15 {
            let mut cue = Cue::new_typed(CueType::Audio);
            if index == 2 {
                cue.pre_wait_secs = 3.0;
            }
            if index == 7 {
                cue.post_wait_secs = 4.0;
            }
            ids.push(cue.id);
            cue_list.add(cue);
        }

        let segments = build_all_cues_segments(&cue_list, &ids);

        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].children.as_slice(), &ids[0..2]);
        assert_eq!(segments[0].pre_wait_secs, 0.0);
        assert_eq!(segments[0].post_wait_secs, 0.0);

        assert_eq!(segments[1].children.as_slice(), &ids[2..8]);
        assert_eq!(segments[1].pre_wait_secs, 3.0);
        assert_eq!(segments[1].post_wait_secs, 4.0);

        assert_eq!(segments[2].children.as_slice(), &ids[8..15]);
        assert_eq!(segments[2].pre_wait_secs, 0.0);
        assert_eq!(segments[2].post_wait_secs, 0.0);
    }

    #[test]
    fn all_zero_wait_children_remain_one_simultaneous_segment() {
        let mut cue_list = CueList::new("All Cues zero-wait test");
        let mut ids = Vec::new();

        for _ in 0..20 {
            let cue = Cue::new_typed(CueType::Audio);
            ids.push(cue.id);
            cue_list.add(cue);
        }

        let segments = build_all_cues_segments(&cue_list, &ids);

        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].children.as_slice(), ids.as_slice());
        assert_eq!(segments[0].pre_wait_secs, 0.0);
        assert_eq!(segments[0].post_wait_secs, 0.0);
    }

    #[test]
    fn consecutive_post_and_pre_waits_remain_separate_barriers() {
        let mut cue_list = CueList::new("All Cues adjacent waits test");
        let mut first = Cue::new_typed(CueType::Audio);
        first.post_wait_secs = 4.0;
        let mut second = Cue::new_typed(CueType::Audio);
        second.pre_wait_secs = 3.0;
        let third = Cue::new_typed(CueType::Audio);
        let ids = vec![first.id, second.id, third.id];

        cue_list.add(first);
        cue_list.add(second);
        cue_list.add(third);

        let segments = build_all_cues_segments(&cue_list, &ids);

        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].children.as_slice(), &ids[0..1]);
        assert_eq!(segments[0].post_wait_secs, 4.0);
        assert_eq!(segments[1].children.as_slice(), &ids[1..3]);
        assert_eq!(segments[1].pre_wait_secs, 3.0);
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
    fn stable_endpoint_id_wins_over_a_matching_display_name() {
        let mut cue = Cue::new_typed(CueType::Audio);
        cue.io.output_backend = Some(AudioBackend::Asio);
        cue.io.output_device = Some("TASCAM US-366".to_string());
        cue.io.output_device_id = Some("asio:old-device".to_string());

        assert!(!cue_uses_output_endpoint(
            &cue,
            AudioBackend::Asio,
            "TASCAM US-366",
            "asio:new-device"
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

#[cfg(test)]
mod recent_project_tests {
    use super::{
        recent_project_hover_path, recent_project_menu_label, remember_recent_project_path,
        MAX_RECENT_PROJECTS,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn recent_projects_are_deduplicated_most_recent_first_and_capped_at_fifteen() {
        let mut recent = Vec::new();
        for index in 0..20 {
            let path = PathBuf::from(format!("project-{index}.tsq"));
            remember_recent_project_path(&mut recent, &path);
        }

        assert_eq!(recent.len(), MAX_RECENT_PROJECTS);
        assert_eq!(recent.first().map(String::as_str), Some("project-19.tsq"));
        assert_eq!(recent.last().map(String::as_str), Some("project-5.tsq"));

        remember_recent_project_path(&mut recent, Path::new("project-10.tsq"));
        assert_eq!(recent.len(), MAX_RECENT_PROJECTS);
        assert_eq!(recent.first().map(String::as_str), Some("project-10.tsq"));
        assert_eq!(
            recent
                .iter()
                .filter(|path| path.as_str() == "project-10.tsq")
                .count(),
            1
        );
    }

    #[test]
    fn recent_project_label_shows_only_the_filename() {
        let label = recent_project_menu_label(Path::new("D:/Shows/Tour/Opening.tsq"));
        assert_eq!(label, "Opening.tsq");
        assert!(!label.contains("Tour"));
        assert!(!label.contains("Shows"));
    }

    #[test]
    fn recent_project_hover_path_hides_windows_verbatim_prefix() {
        let path = Path::new(r"\\?\C:\Users\Ira\Desktop\Opening.tsq");
        assert_eq!(
            recent_project_hover_path(path),
            r"C:\Users\Ira\Desktop\Opening.tsq"
        );
    }

    #[test]
    fn recent_project_hover_path_formats_verbatim_unc_prefix() {
        let path = Path::new(r"\\?\UNC\server\shows\Opening.tsq");
        assert_eq!(
            recent_project_hover_path(path),
            r"\\server\shows\Opening.tsq"
        );
    }

    #[test]
    fn recent_project_hover_path_keeps_normal_paths_unchanged() {
        let path = Path::new(r"D:\Shows\Opening.tsq");
        assert_eq!(recent_project_hover_path(path), r"D:\Shows\Opening.tsq");
    }
}

#[cfg(test)]
mod project_close_snapshot_tests {
    use super::serialize_project_snapshot;
    use crate::cue::{Cue, CueList, CueState, CueType};
    use crate::project::save_load::Project;

    #[test]
    fn runtime_playback_state_does_not_mark_project_dirty() {
        let mut cue_list = CueList::new("Main");
        let cue = Cue::new_typed(CueType::Audio);
        cue_list.add(cue);

        let project = Project::default();
        let saved = serialize_project_snapshot(&project, &cue_list)
            .expect("initial project snapshot serializes");

        let mut changed_runtime = cue_list.clone();
        changed_runtime.cues[0].state = CueState::Playing;
        changed_runtime.cues[0].position_secs = 12.5;
        let current = serialize_project_snapshot(&project, &changed_runtime)
            .expect("runtime project snapshot serializes");

        assert_eq!(saved, current);
    }

    #[test]
    fn cue_edits_are_detected_by_project_snapshot() {
        let mut cue_list = CueList::new("Main");
        cue_list.add(Cue::new_typed(CueType::Audio));
        let project = Project::default();
        let saved = serialize_project_snapshot(&project, &cue_list)
            .expect("initial project snapshot serializes");

        cue_list.cues[0].name = "Changed cue name".to_string();
        let current = serialize_project_snapshot(&project, &cue_list)
            .expect("edited project snapshot serializes");

        assert_ne!(saved, current);
    }

    #[test]
    fn project_settings_edits_are_detected_by_project_snapshot() {
        let cue_list = CueList::new("Main");
        let project = Project::default();
        let saved = serialize_project_snapshot(&project, &cue_list)
            .expect("initial project snapshot serializes");

        let mut changed_project = project.clone();
        changed_project.settings.author = "Stage Manager".to_string();
        let current = serialize_project_snapshot(&changed_project, &cue_list)
            .expect("edited project snapshot serializes");

        assert_ne!(saved, current);
    }
}
