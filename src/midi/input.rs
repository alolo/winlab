// MIDI input — listens for Note On, CC, SysEx from connected controllers

use crossbeam_channel::{bounded, Receiver, Sender};
use midir::{MidiInput, MidiInputConnection};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MidiMessage {
    NoteOn { channel: u8, note: u8, velocity: u8 },
    NoteOff { channel: u8, note: u8 },
    CC { channel: u8, cc: u8, value: u8 },
    SysEx(Vec<u8>),
}

pub struct MidiInputManager {
    pub msg_rx: Receiver<MidiMessage>,
    pub port_names: Vec<String>,
    _connection: Option<MidiInputConnection<()>>,
}

impl MidiInputManager {
    pub fn new() -> Self {
        let (tx, rx) = bounded::<MidiMessage>(512);
        let (port_names, conn) = Self::try_connect(tx);
        Self {
            msg_rx: rx,
            port_names,
            _connection: conn,
        }
    }

    fn try_connect(tx: Sender<MidiMessage>) -> (Vec<String>, Option<MidiInputConnection<()>>) {
        let midi_in = match MidiInput::new("CueLabs Input") {
            Ok(m) => m,
            Err(e) => {
                warn!("MIDI init failed: {e}");
                return (vec![], None);
            }
        };

        let ports = midi_in.ports();
        let names: Vec<String> = ports
            .iter()
            .filter_map(|p| midi_in.port_name(p).ok())
            .collect();

        info!("MIDI ports: {:?}", names);

        // Connect to first available port
        let port = match ports.first() {
            Some(p) => p.clone(),
            None => return (names, None),
        };

        let port_name = midi_in.port_name(&port).unwrap_or_default();
        info!("Connecting to MIDI port: {port_name}");

        let conn = midi_in
            .connect(
                &port,
                "cuelabs-in",
                move |_stamp, raw, _| {
                    if let Some(msg) = parse_midi(raw) {
                        let _ = tx.try_send(msg);
                    }
                },
                (),
            )
            .ok();

        (names, conn)
    }

    pub fn drain(&self) -> Vec<MidiMessage> {
        let mut out = Vec::new();
        while let Ok(m) = self.msg_rx.try_recv() {
            out.push(m);
        }
        out
    }
}

fn parse_midi(raw: &[u8]) -> Option<MidiMessage> {
    if raw.is_empty() {
        return None;
    }
    let status = raw[0];
    let kind = status & 0xF0;
    let channel = status & 0x0F;

    match kind {
        0x90 if raw.len() >= 3 && raw[2] > 0 => Some(MidiMessage::NoteOn {
            channel,
            note: raw[1],
            velocity: raw[2],
        }),
        0x80 | 0x90 if raw.len() >= 3 => Some(MidiMessage::NoteOff {
            channel,
            note: raw[1],
        }),
        0xB0 if raw.len() >= 3 => Some(MidiMessage::CC {
            channel,
            cc: raw[1],
            value: raw[2],
        }),
        0xF0 => Some(MidiMessage::SysEx(raw.to_vec())),
        _ => None,
    }
}
