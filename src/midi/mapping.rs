// MIDI CC mapping — maps MIDI messages to application actions
#![allow(dead_code)]
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MidiAction {
    PlayCue(Uuid),
    StopCue(Uuid),
    PauseCue(Uuid),
    ResumeCue(Uuid),
    SetVolume(Uuid), // value from CC
    StopAll,
    PauseAll,
    ResumeAll,
    Go, // play next/selected cue
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MidiKey {
    pub channel: u8,
    pub cc: u8,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct MidiMapping {
    pub cc_map: HashMap<MidiKey, MidiAction>,
    pub note_map: HashMap<(u8, u8), MidiAction>, // (channel, note) -> action
}

impl MidiMapping {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_cc(&mut self, channel: u8, cc: u8, action: MidiAction) {
        self.cc_map.insert(MidiKey { channel, cc }, action);
    }

    pub fn add_note(&mut self, channel: u8, note: u8, action: MidiAction) {
        self.note_map.insert((channel, note), action);
    }

    pub fn resolve_cc(&self, channel: u8, cc: u8) -> Option<&MidiAction> {
        self.cc_map.get(&MidiKey { channel, cc })
    }

    pub fn resolve_note(&self, channel: u8, note: u8) -> Option<&MidiAction> {
        self.note_map.get(&(channel, note))
    }
}
