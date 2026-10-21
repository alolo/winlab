// Core data structures for cues

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// Playback state of a single cue
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CueState {
    Stopped,
    Playing,
    Paused,
    FadingIn,
    FadingOut,
    Waiting, // armed, ready to go
}

impl Default for CueState {
    fn default() -> Self {
        CueState::Stopped
    }
}

/// Colour tag for visual grouping
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CueColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl CueColor {
    pub const NONE: Self = Self { r: 40, g: 40, b: 48 };
    pub const RED: Self = Self { r: 200, g: 60, b: 60 };
    pub const GREEN: Self = Self { r: 60, g: 180, b: 80 };
    pub const BLUE: Self = Self { r: 60, g: 120, b: 220 };
    pub const YELLOW: Self = Self { r: 220, g: 180, b: 40 };
    pub const PURPLE: Self = Self { r: 150, g: 60, b: 200 };
    pub const ORANGE: Self = Self { r: 220, g: 120, b: 40 };

    pub fn to_egui(self) -> egui::Color32 {
        egui::Color32::from_rgb(self.r, self.g, self.b)
    }
}

use egui::Color32;

/// Loop region within an audio file
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopRegion {
    pub start_secs: f64,
    pub end_secs: f64,
    /// None = infinite loop
    pub count: Option<u32>,
    pub enabled: bool,
}

impl Default for LoopRegion {
    fn default() -> Self {
        Self {
            start_secs: 0.0,
            end_secs: 10.0,
            count: None,
            enabled: false,
        }
    }
}

/// Fade curve shape
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FadeCurve {
    Linear,
    Logarithmic,
    SCurve,
}

impl Default for FadeCurve {
    fn default() -> Self {
        FadeCurve::Logarithmic
    }
}

/// MIDI trigger definition
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MidiTrigger {
    NoteOn { channel: u8, note: u8 },
    CC { channel: u8, cc: u8, value_threshold: u8 },
}

/// Keyboard hotkey
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hotkey {
    pub key: String, // e.g. "F1", "Space", "1"
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

/// Output channel routing (for multi-channel devices)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelRoute {
    pub left: usize,
    pub right: usize,
}

impl Default for ChannelRoute {
    fn default() -> Self {
        Self { left: 0, right: 1 }
    }
}

/// A single audio cue — the core data unit of CueLabs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioCue {
    pub id: Uuid,
    pub number: String,     // "1", "1.5", "A1" — user-editable
    pub name: String,
    pub notes: String,

    // File
    pub file_path: PathBuf,
    pub file_path_relative: Option<PathBuf>, // relative to project

    // Playback settings
    pub volume: f32,        // 0.0 .. 2.0 (1.0 = 100%)
    pub fade_in_secs: f32,
    pub fade_out_secs: f32,
    pub fade_curve: FadeCurve,
    pub pre_delay_secs: f32,

    // Region & loop
    pub start_secs: f64,    // playback start marker
    pub end_secs: Option<f64>, // None = end of file
    pub loop_region: LoopRegion,

    // Triggers
    pub hotkey: Option<Hotkey>,
    pub midi_trigger: Option<MidiTrigger>,

    // Routing
    pub channel_route: ChannelRoute,

    // Visual
    pub color: CueColor,

    // Runtime state (not serialized)
    #[serde(skip)]
    pub state: CueState,
    #[serde(skip)]
    pub position_secs: f64,
    #[serde(skip)]
    pub duration_secs: f64,
    #[serde(skip)]
    pub waveform: Vec<f32>, // normalized peak data for display
}

impl AudioCue {
    pub fn new(file_path: PathBuf) -> Self {
        let name = file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Untitled")
            .to_string();

        Self {
            id: Uuid::new_v4(),
            number: String::new(),
            name,
            notes: String::new(),
            file_path,
            file_path_relative: None,
            volume: 1.0,
            fade_in_secs: 0.0,
            fade_out_secs: 0.0,
            fade_curve: FadeCurve::default(),
            pre_delay_secs: 0.0,
            start_secs: 0.0,
            end_secs: None,
            loop_region: LoopRegion::default(),
            hotkey: None,
            midi_trigger: None,
            channel_route: ChannelRoute::default(),
            color: CueColor::NONE,
            state: CueState::Stopped,
            position_secs: 0.0,
            duration_secs: 0.0,
            waveform: Vec::new(),
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self.state,
            CueState::Playing | CueState::FadingIn | CueState::FadingOut
        )
    }

    pub fn progress(&self) -> f32 {
        if self.duration_secs > 0.0 {
            (self.position_secs / self.duration_secs).clamp(0.0, 1.0) as f32
        } else {
            0.0
        }
    }

    pub fn remaining_secs(&self) -> f64 {
        (self.duration_secs - self.position_secs).max(0.0)
    }
}
