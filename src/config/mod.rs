// App config — hotkeys and global settings

use crate::cue::OutputChannelSetting;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AppAction {
    Go,
    StopSelected,
    StopAll,
    PauseSelected,
    PauseAll,
    ResumeSelected,
    ResumeAll,
    SelectNext,
    SelectPrev,
    NewProject,
    OpenProject,
    SaveProject,
    AddFile,
    DeleteSelected,
    RenameSelected,
    MoveUp,
    MoveDown,
}

impl AppAction {
    pub const ALL: &'static [AppAction] = &[
        AppAction::Go,
        AppAction::StopSelected,
        AppAction::StopAll,
        AppAction::PauseSelected,
        AppAction::PauseAll,
        AppAction::ResumeSelected,
        AppAction::ResumeAll,
        AppAction::SelectNext,
        AppAction::SelectPrev,
        AppAction::NewProject,
        AppAction::OpenProject,
        AppAction::SaveProject,
        AppAction::AddFile,
        AppAction::DeleteSelected,
        AppAction::RenameSelected,
        AppAction::MoveUp,
        AppAction::MoveDown,
    ];

    /// Actions exposed in Settings > MIDI Control. File/project and Cue List
    /// editing commands remain available through application hotkeys, but are
    /// intentionally excluded from MIDI learn to avoid accidental destructive
    /// operations during a live show.
    pub const MIDI_CONTROL_ACTIONS: &'static [AppAction] = &[
        AppAction::Go,
        AppAction::StopSelected,
        AppAction::StopAll,
        AppAction::PauseSelected,
        AppAction::PauseAll,
        AppAction::ResumeSelected,
        AppAction::ResumeAll,
        AppAction::SelectNext,
        AppAction::SelectPrev,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Self::Go => "GO (play selected)",
            Self::StopSelected => "Stop selected",
            Self::StopAll => "Stop all",
            Self::PauseSelected => "Pause selected",
            Self::PauseAll => "Pause all",
            Self::ResumeSelected => "Resume selected",
            Self::ResumeAll => "Resume all",
            Self::SelectNext => "Select next",
            Self::SelectPrev => "Select previous",
            Self::NewProject => "New project",
            Self::OpenProject => "Open project",
            Self::SaveProject => "Save project",
            Self::AddFile => "Add file(s)",
            Self::DeleteSelected => "Delete selected",
            Self::RenameSelected => "Rename selected",
            Self::MoveUp => "Move cue up",
            Self::MoveDown => "Move cue down",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MidiControlKind {
    NoteOn,
    ControlChange,
}

impl MidiControlKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::NoteOn => "Note",
            Self::ControlChange => "CC",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidiControlBinding {
    /// MIDI channel in the user-facing 1..=16 range.
    pub channel: u8,
    pub kind: MidiControlKind,
    /// Note number for NoteOn, or controller number for ControlChange.
    pub number: u8,
    pub action: AppAction,
}

impl MidiControlBinding {
    pub fn label(&self) -> String {
        format!(
            "Ch {} · {} {}",
            self.channel,
            self.kind.label(),
            self.number
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AudioBackend {
    /// Windows shared-mode audio through the system audio engine.
    #[default]
    Wasapi,
    /// Windows low-latency direct hardware backend. Requires an installed ASIO driver.
    Asio,
}

impl AudioBackend {
    pub const ALL: &'static [AudioBackend] = &[AudioBackend::Wasapi, AudioBackend::Asio];

    pub fn label(self) -> &'static str {
        match self {
            AudioBackend::Wasapi => {
                if cfg!(target_os = "windows") {
                    "WASAPI (Shared)"
                } else {
                    "System Default"
                }
            }
            AudioBackend::Asio => "ASIO",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemePreset {
    #[default]
    Dark,
    Light,
    Black,
    Custom,
}

impl ThemePreset {
    pub const ALL: &'static [ThemePreset] = &[
        ThemePreset::Dark,
        ThemePreset::Light,
        ThemePreset::Black,
        ThemePreset::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ThemePreset::Dark => "Dark",
            ThemePreset::Light => "Light",
            ThemePreset::Black => "Black",
            ThemePreset::Custom => "Custom",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    pub preset: ThemePreset,
    /// RGB values for the main application window background.
    pub window_color: [u8; 3],
    /// RGB values for panels, dialogs and inspector surfaces.
    pub panel_color: [u8; 3],
    /// Primary text color, including menu labels.
    pub text_color: [u8; 3],
    /// Secondary/informational text color.
    pub muted_text_color: [u8; 3],
    /// Fill and waveform tint used for the active loop region.
    #[serde(default = "default_loop_region_color")]
    pub loop_region_color: [u8; 3],
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            preset: ThemePreset::Dark,
            window_color: [18, 18, 22],
            panel_color: [26, 26, 32],
            text_color: [220, 220, 230],
            muted_text_color: [150, 150, 165],
            loop_region_color: [75, 155, 245],
        }
    }
}

fn default_loop_region_color() -> [u8; 3] {
    [75, 155, 245]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    pub bindings: HashMap<AppAction, String>,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        let mut bindings = HashMap::new();
        bindings.insert(AppAction::Go, "Space".to_string());
        bindings.insert(AppAction::StopSelected, "S".to_string());
        bindings.insert(AppAction::StopAll, "Escape".to_string());
        bindings.insert(AppAction::PauseSelected, "P".to_string());
        bindings.insert(AppAction::PauseAll, "Shift+P".to_string());
        bindings.insert(AppAction::ResumeSelected, "R".to_string());
        bindings.insert(AppAction::ResumeAll, "Shift+R".to_string());
        bindings.insert(AppAction::SelectNext, "Down".to_string());
        bindings.insert(AppAction::SelectPrev, "Up".to_string());
        bindings.insert(AppAction::SaveProject, "Ctrl+S".to_string());
        bindings.insert(AppAction::OpenProject, "Ctrl+O".to_string());
        bindings.insert(AppAction::NewProject, "Ctrl+N".to_string());
        bindings.insert(AppAction::AddFile, "Ctrl+I".to_string());
        bindings.insert(AppAction::DeleteSelected, "Delete".to_string());
        bindings.insert(AppAction::RenameSelected, "F2".to_string());
        bindings.insert(AppAction::MoveUp, "Alt+Up".to_string());
        bindings.insert(AppAction::MoveDown, "Alt+Down".to_string());
        Self { bindings }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub hotkeys: HotkeyConfig,
    #[serde(default)]
    pub appearance: AppearanceSettings,
    #[serde(default)]
    pub audio_backend: AudioBackend,
    pub audio_output_device: Option<String>,
    pub audio_input_device: Option<String>,
    pub osc_listen_port: u16,
    pub osc_send_addr: String,
    pub midi_input_port: Option<String>,
    pub midi_output_port: Option<String>,
    #[serde(default)]
    pub midi_controls: Vec<MidiControlBinding>,
    pub output_channels: Vec<OutputChannelSetting>,
    /// Optional user-defined aliases for each device's stream channels.
    /// Native ASIO channel names are queried separately from the driver.
    #[serde(default)]
    pub output_channel_names: HashMap<String, Vec<String>>,
    /// Default stereo left/right stream indices for each output device.
    #[serde(default)]
    pub default_output_pairs: HashMap<String, [usize; 2]>,
    /// Logical CueLabs output order mapped to native driver stream-channel indices.
    /// Each vector is a permutation, keyed by backend and device name.
    #[serde(default)]
    pub output_channel_orders: HashMap<String, Vec<usize>>,
    pub stop_all_fade_secs: f64,
    /// User-resized Cue List column widths, in logical points.
    #[serde(default = "default_cue_column_widths")]
    pub cue_column_widths: [f32; 9],
    /// User-resized lower inspector panel height, in logical points.
    #[serde(default = "default_bottom_panel_height")]
    pub bottom_panel_height: f32,
    /// Periodic project backup interval in minutes. Zero disables timed
    /// backups; a backup is still created after every successful save.
    #[serde(default = "default_backup_interval_minutes")]
    pub backup_interval_minutes: u32,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            hotkeys: HotkeyConfig::default(),
            appearance: AppearanceSettings::default(),
            audio_backend: AudioBackend::Wasapi,
            audio_output_device: None,
            audio_input_device: None,
            osc_listen_port: 7700,
            osc_send_addr: "127.0.0.1:7701".to_string(),
            midi_input_port: None,
            midi_output_port: None,
            midi_controls: Vec::new(),
            output_channels: Vec::new(),
            output_channel_names: HashMap::new(),
            default_output_pairs: HashMap::new(),
            output_channel_orders: HashMap::new(),
            stop_all_fade_secs: 0.0,
            cue_column_widths: default_cue_column_widths(),
            bottom_panel_height: default_bottom_panel_height(),
            backup_interval_minutes: default_backup_interval_minutes(),
        }
    }
}

fn default_cue_column_widths() -> [f32; 9] {
    // Status is merged into Duration; slot 7 is retained for compatibility
    // with existing 9-column settings files but defaults to zero.
    [44.0, 75.0, 210.0, 165.0, 68.0, 173.0, 68.0, 0.0, 125.0]
}

fn default_bottom_panel_height() -> f32 {
    225.0
}

fn default_backup_interval_minutes() -> u32 {
    10
}

impl AppSettings {
    fn file_path() -> Option<std::path::PathBuf> {
        dirs::config_dir().map(|dir| dir.join("CueLabs").join("settings.json"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::file_path() else {
            return Self::default();
        };
        let mut settings: Self = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();

        // Remove legacy MIDI mappings for project/file/Cue List editing actions
        // that are no longer offered in MIDI Control. These actions remain in
        // AppAction::ALL and therefore remain available to keyboard shortcuts.
        let previous_binding_count = settings.midi_controls.len();
        settings
            .midi_controls
            .retain(|binding| AppAction::MIDI_CONTROL_ACTIONS.contains(&binding.action));
        if settings.midi_controls.len() != previous_binding_count {
            // Persist the one-time settings migration. Failure is non-fatal;
            // the unsupported bindings are still filtered in memory this run.
            let _ = settings.save();
        }

        settings
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::file_path()
            .ok_or_else(|| anyhow::anyhow!("Cannot resolve application config directory"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod midi_control_action_tests {
    use super::AppAction;

    #[test]
    fn midi_control_excludes_project_and_cue_list_editing_actions() {
        let excluded = [
            AppAction::NewProject,
            AppAction::OpenProject,
            AppAction::SaveProject,
            AppAction::AddFile,
            AppAction::DeleteSelected,
            AppAction::RenameSelected,
            AppAction::MoveUp,
            AppAction::MoveDown,
        ];

        for action in excluded {
            assert!(!AppAction::MIDI_CONTROL_ACTIONS.contains(&action));
            // These commands remain part of the global action set for hotkeys.
            assert!(AppAction::ALL.contains(&action));
        }

        assert_eq!(AppAction::MIDI_CONTROL_ACTIONS.len(), 9);
    }
}
