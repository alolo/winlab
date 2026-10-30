// CueLabs — расширенная модель кью с типами, timing и I/O

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

// ── Типы кью ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CueType {
    /// Воспроизведение аудиофайла
    Audio,
    /// Пауза целевого кью (или всех)
    Pause { target_id: Option<Uuid> },
    /// Запуск целевого кью
    Play  { target_id: Option<Uuid> },
    /// Остановка целевого кью
    Stop  { target_id: Option<Uuid> },
    /// Плавное нарастание / затухание
    Fade  { target_id: Option<Uuid>, fade_in: bool, duration_secs: f64 },
    /// Отправка MIDI-сообщения
    Midi  { channel: u8, message: MidiMessageKind },
    /// Группа кью
    Group { mode: GroupMode, children: Vec<Uuid> },
}

impl CueType {
    pub fn label(&self) -> &'static str {
        match self {
            CueType::Audio        => "Audio",
            CueType::Pause { .. } => "Pause",
            CueType::Play  { .. } => "Play",
            CueType::Stop  { .. } => "Stop",
            CueType::Fade  { .. } => "Fade",
            CueType::Midi  { .. } => "MIDI",
            CueType::Group { .. } => "Group",
        }
    }
    pub fn icon(&self) -> &'static str {
        match self {
            CueType::Audio        => "🔊",
            CueType::Pause { .. } => "⏸",
            CueType::Play  { .. } => "▶",
            CueType::Stop  { .. } => "⏹",
            CueType::Fade  { fade_in, .. } => if *fade_in { "↗" } else { "↘" },
            CueType::Midi  { .. } => "🎹",
            CueType::Group { .. } => "📁",
        }
    }
}

// ── MIDI типы сообщений ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MidiMessageKind {
    NoteOn          { note: u8, velocity: u8 },
    NoteOff         { note: u8, velocity: u8 },
    ControlChange   { cc: u8, value: u8 },
    ProgramChange   { program: u8 },
    PitchBend       { value: i16 },
    Raw(Vec<u8>),
}

impl MidiMessageKind {
    pub fn label(&self) -> &'static str {
        match self {
            MidiMessageKind::NoteOn { .. }        => "Note On",
            MidiMessageKind::NoteOff { .. }       => "Note Off",
            MidiMessageKind::ControlChange { .. } => "Control Change",
            MidiMessageKind::ProgramChange { .. } => "Program Change",
            MidiMessageKind::PitchBend { .. }     => "Pitch Bend",
            MidiMessageKind::Raw(_)               => "Raw Bytes",
        }
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            MidiMessageKind::NoteOn  { note, velocity }        => vec![0x90, *note, *velocity],
            MidiMessageKind::NoteOff { note, velocity }        => vec![0x80, *note, *velocity],
            MidiMessageKind::ControlChange { cc, value }       => vec![0xB0, *cc, *value],
            MidiMessageKind::ProgramChange { program }         => vec![0xC0, *program],
            MidiMessageKind::PitchBend { value } => {
                let v = (*value + 8192) as u16;
                vec![0xE0, (v & 0x7F) as u8, ((v >> 7) & 0x7F) as u8]
            }
            MidiMessageKind::Raw(b)                            => b.clone(),
        }
    }
}

// ── Режим группы ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum GroupMode {
    /// Все кью запускаются одновременно
    Simultaneous,
    /// Кью запускаются по очереди
    Sequential,
}

// ── Continue mode ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum ContinueMode {
    #[default]
    Nothing,
    /// Следующий кью запускается сразу после начала текущего
    AutoContinue,
    /// Следующий кью запускается сразу после окончания текущего
    AutoFollow,
}

impl ContinueMode {
    pub fn label(&self) -> &'static str {
        match self {
            ContinueMode::Nothing      => "—",
            ContinueMode::AutoContinue => "Auto-Continue",
            ContinueMode::AutoFollow   => "Auto-Follow",
        }
    }
    pub const ALL: &'static [ContinueMode] = &[
        ContinueMode::Nothing,
        ContinueMode::AutoContinue,
        ContinueMode::AutoFollow,
    ];
}

// ── Состояние кью ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum CueState {
    #[default]
    Stopped,
    Playing,
    Paused,
    Loading,
    Error(String),
}

impl CueState {
    pub fn is_active(&self) -> bool {
        matches!(self, CueState::Playing | CueState::Paused | CueState::Loading)
    }
}

// ── Loop Region ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LoopRegion {
    pub enabled:   bool,
    pub start_secs: f64,
    pub end_secs:   f64,
    pub count:      i32, // -1 = infinite
}

// ── I/O Routing ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct IoRouting {
    /// None = use project default
    pub output_device: Option<String>,
    pub input_device:  Option<String>,
    /// Output channel offset (for Dante multi-channel)
    pub output_channel: u16,
}

// ── Fade curve ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum FadeCurve {
    #[default]
    Linear,
    SCurve,
    Logarithmic,
    Exponential,
}

// ── Цвет кью ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum CueColor {
    #[default]
    None,
    Red, Orange, Yellow, Green, Cyan, Blue, Purple, Pink, White,
}

impl CueColor {
    /// Яркий цвет для индикатора
    pub fn to_rgb(&self) -> [u8; 3] {
        match self {
            CueColor::None   => [60, 60, 75],
            CueColor::Red    => [220, 60, 60],
            CueColor::Orange => [220, 140, 40],
            CueColor::Yellow => [220, 210, 40],
            CueColor::Green  => [60, 200, 80],
            CueColor::Cyan   => [40, 200, 200],
            CueColor::Blue   => [60, 130, 230],
            CueColor::Purple => [160, 80, 220],
            CueColor::Pink   => [220, 80, 160],
            CueColor::White  => [200, 200, 210],
        }
    }
    /// Приглушённый цвет для фона строки (50% alpha)
    pub fn row_bg_rgba(&self) -> [u8; 4] {
        let [r, g, b] = self.to_rgb();
        if matches!(self, CueColor::None) {
            [0, 0, 0, 0] // прозрачный
        } else {
            [r, g, b, 70]
        }
    }
    pub const ALL: &'static [CueColor] = &[
        CueColor::None, CueColor::Red, CueColor::Orange, CueColor::Yellow,
        CueColor::Green, CueColor::Cyan, CueColor::Blue, CueColor::Purple,
        CueColor::Pink, CueColor::White,
    ];
    pub fn label(&self) -> &'static str {
        match self {
            CueColor::None   => "None",
            CueColor::Red    => "Red",
            CueColor::Orange => "Orange",
            CueColor::Yellow => "Yellow",
            CueColor::Green  => "Green",
            CueColor::Cyan   => "Cyan",
            CueColor::Blue   => "Blue",
            CueColor::Purple => "Purple",
            CueColor::Pink   => "Pink",
            CueColor::White  => "White",
        }
    }
}

// ── MIDI trigger (входящий) ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MidiTrigger {
    NoteOn  { channel: u8, note: u8 },
    CC      { channel: u8, cc: u8, value: Option<u8> },
}

// ── Главная структура кью ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cue {
    pub id:     Uuid,
    pub number: String,
    pub name:   String,
    pub color:  CueColor,
    pub notes:  String,
    pub armed:  bool,

    // Тип и тип-специфичные данные
    pub cue_type: CueType,

    // Timing
    pub pre_wait_secs:  f64,
    pub post_wait_secs: f64,
    pub continue_mode:  ContinueMode,

    // Состояние (runtime, не сериализуется)
    #[serde(skip)]
    pub state:        CueState,
    #[serde(skip)]
    pub position_secs: f64,

    // ── Audio-specific (используется при CueType::Audio/Play/Pause/Stop/Fade) ──
    /// Путь к аудиофайлу (для Audio кью — основной, для остальных — target)
    pub file_path:    PathBuf,
    pub volume:       f32,
    pub fade_in_secs:  f64,
    pub fade_out_secs: f64,
    pub fade_curve:    FadeCurve,
    pub start_secs:    f64,
    pub loop_region:   LoopRegion,

    // I/O routing
    pub io: IoRouting,

    // MIDI trigger (входящий триггер на этот кью)
    pub midi_trigger: Option<MidiTrigger>,

    // Waveform cache (runtime)
    #[serde(skip)]
    pub waveform:     Vec<f32>,
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
            id:            Uuid::new_v4(),
            number:        String::new(),
            name,
            color:         CueColor::None,
            notes:         String::new(),
            armed:         true,
            cue_type:      CueType::Audio,
            pre_wait_secs:  0.0,
            post_wait_secs: 0.0,
            continue_mode:  ContinueMode::Nothing,
            state:          CueState::Stopped,
            position_secs:  0.0,
            file_path,
            volume:         1.0,
            fade_in_secs:   0.0,
            fade_out_secs:  0.0,
            fade_curve:     FadeCurve::Linear,
            start_secs:     0.0,
            loop_region:    LoopRegion::default(),
            io:             IoRouting::default(),
            midi_trigger:   None,
            waveform:       vec![],
            duration_secs:  0.0,
        }
    }

    pub fn new_typed(cue_type: CueType) -> Self {
        let name = cue_type.label().to_string();
        Self {
            id:            Uuid::new_v4(),
            number:        String::new(),
            name,
            color:         CueColor::None,
            notes:         String::new(),
            armed:         true,
            cue_type,
            pre_wait_secs:  0.0,
            post_wait_secs: 0.0,
            continue_mode:  ContinueMode::Nothing,
            state:          CueState::Stopped,
            position_secs:  0.0,
            file_path:      PathBuf::new(),
            volume:         1.0,
            fade_in_secs:   0.0,
            fade_out_secs:  0.0,
            fade_curve:     FadeCurve::Linear,
            start_secs:     0.0,
            loop_region:    LoopRegion::default(),
            io:             IoRouting::default(),
            midi_trigger:   None,
            waveform:       vec![],
            duration_secs:  0.0,
        }
    }

    pub fn is_active(&self) -> bool {
        self.state.is_active()
    }
}
