// src/cue/cue.rs

use egui::Color32;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CueType {
    Audio,
    Pause {
        target_id: Option<Uuid>,
    },
    Play {
        target_id: Option<Uuid>,
    },
    Stop {
        target_id: Option<Uuid>,
    },
    Fade {
        target_id: Option<Uuid>,
        fade_in: bool,
        duration_secs: f64,
        stop_on_finish: bool,
        /// None follows the target audio cue's configured level for Fade In, or 0 for Fade Out.
        #[serde(default)]
        target_volume: Option<f32>,
        #[serde(default)]
        fade_curve: FadeCurve,
        #[serde(default)]
        custom_fade_points: Vec<[f32; 2]>,
    },
    Midi {
        channel: u8,
        message: MidiMessageKind,
        raw_hex: String,
    },
    Osc {
        address: String,
        #[serde(default)]
        args: Vec<OscCueArg>,
    },
    Group {
        mode: GroupMode,
        children: Vec<Uuid>,
        #[serde(default)]
        fade_in_secs: f64,
        #[serde(default)]
        fade_out_secs: f64,
    },
}

impl CueType {
    pub fn label(&self) -> &'static str {
        match self {
            CueType::Audio => "Audio",
            CueType::Pause { .. } => "Pause",
            CueType::Play { .. } => "Play",
            CueType::Stop { .. } => "Stop",
            CueType::Fade { fade_in, .. } => {
                if *fade_in {
                    "Fade In"
                } else {
                    "Fade Out"
                }
            }
            CueType::Midi { .. } => "MIDI",
            CueType::Osc { .. } => "OSC",
            CueType::Group { .. } => "Group",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            CueType::Audio => "🔊",
            CueType::Pause { .. } => "⏸",
            CueType::Play { .. } => "▶",
            CueType::Stop { .. } => "⏹",
            CueType::Fade { fade_in, .. } => {
                if *fade_in {
                    "↗"
                } else {
                    "↘"
                }
            }
            CueType::Midi { .. } => "🎹",
            CueType::Osc { .. } => "📡",
            CueType::Group { .. } => "📁",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum OscCueArg {
    Int(i32),
    Float(f32),
    String(String),
    Bool(bool),
}

impl OscCueArg {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Int(_) => "Integer",
            Self::Float(_) => "Float",
            Self::String(_) => "String",
            Self::Bool(_) => "Boolean",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MidiMessageKind {
    NoteOn { note: u8, velocity: u8 },
    NoteOff { note: u8, velocity: u8 },
    ControlChange { cc: u8, value: u8 },
    ProgramChange { program: u8 },
    PitchBend { value: i16 },
    Raw(Vec<u8>),
}

impl MidiMessageKind {
    pub fn label(&self) -> &'static str {
        match self {
            MidiMessageKind::NoteOn { .. } => "Note On",
            MidiMessageKind::NoteOff { .. } => "Note Off",
            MidiMessageKind::ControlChange { .. } => "Control Change",
            MidiMessageKind::ProgramChange { .. } => "Program Change",
            MidiMessageKind::PitchBend { .. } => "Pitch Bend",
            MidiMessageKind::Raw(_) => "Raw MIDI Code",
        }
    }

    pub fn to_bytes(&self, channel: u8) -> Vec<u8> {
        let ch = channel.saturating_sub(1) & 0x0F;
        match self {
            MidiMessageKind::NoteOn { note, velocity } => {
                vec![0x90 | ch, *note & 0x7F, *velocity & 0x7F]
            }
            MidiMessageKind::NoteOff { note, velocity } => {
                vec![0x80 | ch, *note & 0x7F, *velocity & 0x7F]
            }
            MidiMessageKind::ControlChange { cc, value } => {
                vec![0xB0 | ch, *cc & 0x7F, *value & 0x7F]
            }
            MidiMessageKind::ProgramChange { program } => vec![0xC0 | ch, *program & 0x7F],
            MidiMessageKind::PitchBend { value } => {
                let v = (*value + 8192).clamp(0, 16383) as u16;
                vec![0xE0 | ch, (v & 0x7F) as u8, ((v >> 7) & 0x7F) as u8]
            }
            MidiMessageKind::Raw(b) => b.clone(),
        }
    }

    pub fn parse_hex(hex_str: &str) -> Vec<u8> {
        hex_str
            .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
            .filter(|s| !s.is_empty())
            .filter_map(|s| {
                let clean = s
                    .strip_prefix("0x")
                    .or_else(|| s.strip_prefix("0X"))
                    .unwrap_or(s);
                u8::from_str_radix(clean, 16).ok()
            })
            .collect()
    }
}

// ==== Режимы воспроизведения групп ====
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum GroupMode {
    /// Запускает только первое событие. Дальше — по auto-continue/follow цепочке детей.
    #[default]
    Normal,
    /// Plays each child one after another, respecting each child's pre-wait and post-wait.
    Playlist,
    /// Starts every child at once (each child still observes its own pre-wait).
    AllCues,
}

impl GroupMode {
    pub fn label(&self) -> &'static str {
        match self {
            GroupMode::Normal => "Normal",
            GroupMode::Playlist => "Playlist",
            GroupMode::AllCues => "All Cues",
        }
    }
    pub const ALL: &'static [GroupMode] =
        &[GroupMode::Normal, GroupMode::Playlist, GroupMode::AllCues];
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum ContinueMode {
    #[default]
    Nothing,
    AutoContinue,
    AutoFollow,
}

impl ContinueMode {
    pub fn label(&self) -> &'static str {
        match self {
            ContinueMode::Nothing => "Nothing",
            ContinueMode::AutoContinue => "Auto-Continue",
            ContinueMode::AutoFollow => "Auto-Follow",
        }
    }
    pub const ALL: &'static [ContinueMode] = &[
        ContinueMode::Nothing,
        ContinueMode::AutoContinue,
        ContinueMode::AutoFollow,
    ];
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum CueState {
    #[default]
    Stopped,
    Waiting,
    Playing,
    FadingIn,
    FadingOut,
    Paused,
    Loading,
    Error(String),
}

impl CueState {
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            CueState::Waiting
                | CueState::Playing
                | CueState::FadingIn
                | CueState::FadingOut
                | CueState::Paused
                | CueState::Loading
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LoopRegion {
    pub enabled: bool,
    pub start_secs: f64,
    pub end_secs: f64,
    pub count: i32,
}

impl Default for LoopRegion {
    fn default() -> Self {
        Self {
            enabled: false,
            start_secs: 0.0,
            end_secs: 0.0,
            count: -1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelRoute {
    pub active: bool,
    /// Zero-based physical output index. The UI displays it as 1-based.
    pub output_channel: usize,
    pub gain_db: f32,
}

impl Default for ChannelRoute {
    fn default() -> Self {
        Self {
            active: true,
            output_channel: 0,
            gain_db: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputChannelSetting {
    pub active: bool,
    pub gain_db: f32,
}

impl Default for OutputChannelSetting {
    fn default() -> Self {
        Self {
            active: true,
            gain_db: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IoRouting {
    pub output_device: Option<String>,
    pub input_device: Option<String>,
    pub output_channel: u16,
    pub left_gain_db: f32,
    pub right_gain_db: f32,
    pub left_enabled: bool,
    pub right_enabled: bool,
    #[serde(default)]
    pub channel_routes: Vec<ChannelRoute>,
}

impl Default for IoRouting {
    fn default() -> Self {
        Self {
            output_device: None,
            input_device: None,
            output_channel: 0,
            left_gain_db: 0.0,
            right_gain_db: 0.0,
            left_enabled: true,
            right_enabled: true,
            channel_routes: Vec::new(),
        }
    }
}

impl IoRouting {
    pub fn ensure_channel_routes(&mut self, input_channels: usize, output_channels: usize) {
        let input_channels = input_channels.max(1);
        let output_channels = output_channels.max(1);
        if self.channel_routes.len() > input_channels {
            self.channel_routes.truncate(input_channels);
        }
        while self.channel_routes.len() < input_channels {
            let index = self.channel_routes.len();
            let (active, gain_db) = match index {
                0 => (self.left_enabled, self.left_gain_db),
                1 => (self.right_enabled, self.right_gain_db),
                _ => (true, 0.0),
            };
            self.channel_routes.push(ChannelRoute {
                active,
                output_channel: if index < output_channels {
                    index
                } else if output_channels == 2 {
                    index % 2
                } else {
                    0
                },
                gain_db,
            });
        }
        for route in &mut self.channel_routes {
            route.output_channel = route.output_channel.min(output_channels - 1);
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
pub enum FadeCurve {
    #[default]
    Linear,
    SCurve,
    Logarithmic,
    Exponential,
    Custom,
}

impl FadeCurve {
    pub fn apply(&self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            FadeCurve::Linear => t,
            FadeCurve::SCurve => t * t * (3.0 - 2.0 * t),
            FadeCurve::Logarithmic => (t * 9.0 + 1.0).log10(),
            FadeCurve::Exponential => t * t,
            FadeCurve::Custom => t,
        }
    }

    pub fn apply_with_points(&self, t: f32, points: &[[f32; 2]]) -> f32 {
        let t = t.clamp(0.0, 1.0);
        if *self != FadeCurve::Custom || points.len() < 2 {
            return self.apply(t);
        }

        let mut previous = points[0];
        for next in points.iter().copied().skip(1) {
            if t <= next[0] {
                let span = (next[0] - previous[0]).max(0.0001);
                let local_t = ((t - previous[0]) / span).clamp(0.0, 1.0);
                return (previous[1] + (next[1] - previous[1]) * local_t).clamp(0.0, 1.0);
            }
            previous = next;
        }
        previous[1].clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
pub enum CueColor {
    #[default]
    None,
    Red,
    Orange,
    Yellow,
    Green,
    Cyan,
    Blue,
    Purple,
    Pink,
    White,
}

impl CueColor {
    pub fn to_rgb(&self) -> [u8; 3] {
        match self {
            CueColor::None => [60, 60, 75],
            CueColor::Red => [220, 60, 60],
            CueColor::Orange => [220, 140, 40],
            CueColor::Yellow => [220, 210, 40],
            CueColor::Green => [60, 200, 80],
            CueColor::Cyan => [40, 200, 200],
            CueColor::Blue => [60, 130, 230],
            CueColor::Purple => [160, 80, 220],
            CueColor::Pink => [220, 80, 160],
            CueColor::White => [200, 200, 210],
        }
    }

    pub fn to_egui(&self) -> Color32 {
        let [r, g, b] = self.to_rgb();
        Color32::from_rgb(r, g, b)
    }

    /// Цвет заливки строки. Alpha = 26 (~10%).
    pub fn row_bg_egui(&self) -> Option<Color32> {
        if matches!(self, CueColor::None) {
            None
        } else {
            let [r, g, b] = self.to_rgb();
            Some(Color32::from_rgba_unmultiplied(r, g, b, 26))
        }
    }

    pub const ALL: &'static [CueColor] = &[
        CueColor::None,
        CueColor::Red,
        CueColor::Orange,
        CueColor::Yellow,
        CueColor::Green,
        CueColor::Cyan,
        CueColor::Blue,
        CueColor::Purple,
        CueColor::Pink,
        CueColor::White,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            CueColor::None => "None",
            CueColor::Red => "Red",
            CueColor::Orange => "Orange",
            CueColor::Yellow => "Yellow",
            CueColor::Green => "Green",
            CueColor::Cyan => "Cyan",
            CueColor::Blue => "Blue",
            CueColor::Purple => "Purple",
            CueColor::Pink => "Pink",
            CueColor::White => "White",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MidiTrigger {
    NoteOn {
        channel: u8,
        note: u8,
    },
    CC {
        channel: u8,
        cc: u8,
        value: Option<u8>,
    },
}

fn default_custom_fade_points() -> Vec<[f32; 2]> {
    vec![[0.0, 0.0], [1.0, 1.0]]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cue {
    pub id: Uuid,
    pub number: String,
    pub name: String,
    pub color: CueColor,
    pub notes: String,
    pub armed: bool,
    pub cue_type: CueType,
    pub pre_wait_secs: f64,
    pub post_wait_secs: f64,
    pub continue_mode: ContinueMode,
    #[serde(skip)]
    pub state: CueState,
    #[serde(skip)]
    pub position_secs: f64,
    pub file_path: PathBuf,
    #[serde(default)]
    pub audio_channels: usize,
    pub volume: f32,
    pub fade_in_secs: f64,
    pub fade_out_secs: f64,
    pub fade_curve: FadeCurve,
    #[serde(default = "default_custom_fade_points")]
    pub custom_fade_points: Vec<[f32; 2]>,
    pub start_secs: f64,
    #[serde(default)]
    pub end_secs: Option<f64>,
    pub loop_region: LoopRegion,
    pub io: IoRouting,
    pub midi_trigger: Option<MidiTrigger>,
    #[serde(default)]
    pub hotkey: Option<String>,
    #[serde(skip)]
    pub waveform: Vec<f32>,
    #[serde(skip)]
    pub duration_secs: f64,
}

impl Cue {
    pub fn new_audio(file_path: PathBuf) -> Self {
        let name = file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Untitled")
            .to_string();
        Self {
            id: Uuid::new_v4(),
            number: String::new(),
            name,
            color: CueColor::None,
            notes: String::new(),
            armed: true,
            cue_type: CueType::Audio,
            pre_wait_secs: 0.0,
            post_wait_secs: 0.0,
            continue_mode: ContinueMode::Nothing,
            state: CueState::Stopped,
            position_secs: 0.0,
            file_path,
            audio_channels: 2,
            volume: 1.0,
            fade_in_secs: 0.0,
            fade_out_secs: 0.0,
            fade_curve: FadeCurve::Linear,
            custom_fade_points: default_custom_fade_points(),
            start_secs: 0.0,
            end_secs: None,
            loop_region: LoopRegion::default(),
            io: IoRouting::default(),
            midi_trigger: None,
            hotkey: None,
            waveform: vec![],
            duration_secs: 0.0,
        }
    }

    pub fn new_typed(cue_type: CueType) -> Self {
        let name = cue_type.label().to_string();
        Self {
            id: Uuid::new_v4(),
            number: String::new(),
            name,
            color: CueColor::None,
            notes: String::new(),
            armed: true,
            cue_type,
            pre_wait_secs: 0.0,
            post_wait_secs: 0.0,
            continue_mode: ContinueMode::Nothing,
            state: CueState::Stopped,
            position_secs: 0.0,
            file_path: PathBuf::new(),
            audio_channels: 0,
            volume: 1.0,
            fade_in_secs: 0.0,
            fade_out_secs: 0.0,
            fade_curve: FadeCurve::Linear,
            custom_fade_points: default_custom_fade_points(),
            start_secs: 0.0,
            end_secs: None,
            loop_region: LoopRegion::default(),
            io: IoRouting::default(),
            midi_trigger: None,
            hotkey: None,
            waveform: vec![],
            duration_secs: 0.0,
        }
    }

    pub fn is_active(&self) -> bool {
        self.state.is_active()
    }

    pub fn effective_end_secs(&self) -> f64 {
        self.end_secs
            .unwrap_or(self.duration_secs)
            .min(self.duration_secs)
            .max(self.start_secs)
    }

    pub fn effective_duration(&self) -> f64 {
        match &self.cue_type {
            CueType::Audio => (self.effective_end_secs() - self.start_secs).max(0.0),
            CueType::Fade { duration_secs, .. } => *duration_secs,
            _ => self.duration_secs,
        }
    }

    pub fn progress(&self) -> f32 {
        let eff_end = self.effective_end_secs();
        let span = (eff_end - self.start_secs).max(0.001);
        if eff_end <= 0.0 {
            0.0
        } else {
            ((self.position_secs - self.start_secs) / span).clamp(0.0, 1.0) as f32
        }
    }

    pub fn remaining_secs(&self) -> f64 {
        (self.effective_end_secs() - self.position_secs).max(0.0)
    }
}

#[cfg(test)]
mod fade_status_tests {
    use super::*;

    #[test]
    fn fade_status_uses_runtime_elapsed_time() {
        let mut cue = Cue::new_typed(CueType::Fade {
            target_id: None,
            fade_in: true,
            duration_secs: 4.0,
            stop_on_finish: false,
            target_volume: None,
            fade_curve: FadeCurve::Linear,
            custom_fade_points: default_custom_fade_points(),
        });
        cue.state = CueState::FadingIn;
        cue.duration_secs = 4.0;
        cue.position_secs = 1.5;

        assert!((cue.progress() - 0.375).abs() < 0.001);
        assert!((cue.remaining_secs() - 2.5).abs() < 0.001);
    }
}
