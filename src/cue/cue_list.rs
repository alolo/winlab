// CueLabs — список кью

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use crate::cue::{Cue, CueState, CueType, MidiMessageKind, GroupMode};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CueList {
    pub name: String,
    pub cues: Vec<Cue>,
}

impl CueList {
    pub fn new(name: &str) -> Self {
        Self { name: name.to_string(), cues: vec![] }
    }

    pub fn add(&mut self, cue: Cue) {
        self.cues.push(cue);
        self.renumber();
    }

    pub fn remove(&mut self, id: Uuid) {
        self.cues.retain(|c| c.id != id);
        self.renumber();
    }

    pub fn find(&self, id: Uuid) -> Option<&Cue> {
        self.cues.iter().find(|c| c.id == id)
    }

    pub fn find_mut(&mut self, id: Uuid) -> Option<&mut Cue> {
        self.cues.iter_mut().find(|c| c.id == id)
    }

    pub fn renumber(&mut self) {
        for (i, cue) in self.cues.iter_mut().enumerate() {
            cue.number = format!("{}", i + 1);
        }
    }

    pub fn active_count(&self) -> usize {
        self.cues.iter().filter(|c| c.is_active()).count()
    }

    pub fn pause_all(&mut self) {
        for c in &mut self.cues {
            if c.state == CueState::Playing {
                c.state = CueState::Paused;
            }
        }
    }

    pub fn resume_all(&mut self) {
        for c in &mut self.cues {
            if c.state == CueState::Paused {
                c.state = CueState::Playing;
            }
        }
    }

    pub fn stop_all(&mut self) {
        for c in &mut self.cues {
            c.state = CueState::Stopped;
            c.position_secs = 0.0;
        }
    }

    pub fn move_up(&mut self, id: Uuid) {
        if let Some(i) = self.cues.iter().position(|c| c.id == id) {
            if i > 0 { self.cues.swap(i, i - 1); self.renumber(); }
        }
    }

    pub fn move_down(&mut self, id: Uuid) {
        let len = self.cues.len();
        if let Some(i) = self.cues.iter().position(|c| c.id == id) {
            if i + 1 < len { self.cues.swap(i, i + 1); self.renumber(); }
        }
    }

    /// Все аудиофайлы в проекте (для выбора target в Pause/Play/Stop/Fade)
    pub fn audio_targets(&self) -> Vec<(Uuid, String)> {
        self.cues.iter()
            .filter(|c| matches!(c.cue_type, CueType::Audio))
            .map(|c| (c.id, format!("{} — {}", c.number, c.name)))
            .collect()
    }
}
