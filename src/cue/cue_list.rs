// Cue list — ordered collection of AudioCues with group operations

use super::cue::{AudioCue, CueState};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CueList {
    pub cues: Vec<AudioCue>,
    pub name: String,
}

impl CueList {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            cues: Vec::new(),
            name: name.into(),
        }
    }

    pub fn add(&mut self, cue: AudioCue) {
        let next_num = self.cues.len() + 1;
        let mut c = cue;
        if c.number.is_empty() {
            c.number = next_num.to_string();
        }
        self.cues.push(c);
    }

    pub fn remove(&mut self, id: Uuid) {
        self.cues.retain(|c| c.id != id);
    }

    pub fn find_mut(&mut self, id: Uuid) -> Option<&mut AudioCue> {
        self.cues.iter_mut().find(|c| c.id == id)
    }

    pub fn find(&self, id: Uuid) -> Option<&AudioCue> {
        self.cues.iter().find(|c| c.id == id)
    }

    pub fn move_up(&mut self, id: Uuid) {
        if let Some(idx) = self.cues.iter().position(|c| c.id == id) {
            if idx > 0 {
                self.cues.swap(idx, idx - 1);
            }
        }
    }

    pub fn move_down(&mut self, id: Uuid) {
        if let Some(idx) = self.cues.iter().position(|c| c.id == id) {
            if idx + 1 < self.cues.len() {
                self.cues.swap(idx, idx + 1);
            }
        }
    }

    // --- Group operations ---

    pub fn stop_all(&mut self) {
        for c in &mut self.cues {
            c.state = CueState::Stopped;
            c.position_secs = 0.0;
        }
    }

    pub fn pause_all(&mut self) {
        for c in &mut self.cues {
            if c.state == CueState::Playing || c.state == CueState::FadingIn {
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

    pub fn active_count(&self) -> usize {
        self.cues.iter().filter(|c| c.is_active()).count()
    }

    pub fn renumber(&mut self) {
        for (i, c) in self.cues.iter_mut().enumerate() {
            c.number = (i + 1).to_string();
        }
    }
}
