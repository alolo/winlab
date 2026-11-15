// src/cue/cue_list.rs — список кью

use crate::cue::{Cue, CueState};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CueList {
    pub name: String,
    pub cues: Vec<Cue>,
}

impl CueList {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            cues: vec![],
        }
    }

    pub fn add(&mut self, mut cue: Cue) {
        if cue.number.trim().is_empty() {
            let next_number = self
                .cues
                .iter()
                .filter_map(|existing| existing.number.trim().parse::<u64>().ok())
                .max()
                .unwrap_or(0)
                .saturating_add(1);
            cue.number = next_number.to_string();
        }
        self.cues.push(cue);
    }

    pub fn remove(&mut self, id: Uuid) {
        // Cue numbers are stable identifiers used by operators and target labels.
        // Removing a cue must not silently renumber the remaining project.
        self.cues.retain(|c| c.id != id);
    }

    pub fn find(&self, id: Uuid) -> Option<&Cue> {
        self.cues.iter().find(|c| c.id == id)
    }

    pub fn find_mut(&mut self, id: Uuid) -> Option<&mut Cue> {
        self.cues.iter_mut().find(|c| c.id == id)
    }

    pub fn active_count(&self) -> usize {
        self.cues.iter().filter(|c| c.is_active()).count()
    }

    pub fn pause_all(&mut self) {
        for c in &mut self.cues {
            if matches!(
                c.state,
                CueState::Playing | CueState::FadingIn | CueState::FadingOut
            ) {
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
            c.clear_wait_runtime();
        }
    }
}
