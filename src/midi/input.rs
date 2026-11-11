// src/midi/input.rs — MIDI вход и MIDI выход для отправки управляющих команд и Raw Hex

use crossbeam_channel::{bounded, Receiver, Sender};
use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use tracing::{info, warn};

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
    pub current_port: Option<String>,
    msg_tx: Sender<MidiMessage>,
    repaint_context: Arc<OnceLock<egui::Context>>,
    _connection: Option<MidiInputConnection<()>>,
}

impl MidiInputManager {
    pub fn new() -> Self {
        let (tx, rx) = bounded::<MidiMessage>(2048);
        let mut manager = Self {
            msg_rx: rx,
            port_names: Vec::new(),
            current_port: None,
            msg_tx: tx,
            repaint_context: Arc::new(OnceLock::new()),
            _connection: None,
        };
        manager.refresh_and_connect(None);
        manager
    }

    /// Attach the UI context so MIDI input can wake an otherwise idle egui app.
    /// The native MIDI callback only enqueues the message and requests a repaint;
    /// all application state changes still happen on the UI thread.
    pub fn set_repaint_context(&mut self, context: egui::Context) {
        let _ = self.repaint_context.set(context);
    }

    pub fn refresh_and_connect(&mut self, preferred_port: Option<&str>) {
        // Dropping the previous connection cleanly disconnects the old input port.
        self._connection = None;
        self.current_port = None;

        let midi_in = match MidiInput::new("Theatre Show Cue Input") {
            Ok(m) => m,
            Err(e) => {
                warn!("MIDI init failed: {e}");
                self.port_names.clear();
                return;
            }
        };

        let ports = midi_in.ports();
        self.port_names = ports
            .iter()
            .filter_map(|p| midi_in.port_name(p).ok())
            .collect();

        let target = preferred_port
            .and_then(|preferred| {
                ports
                    .iter()
                    .find(|port| midi_in.port_name(port).ok().as_deref() == Some(preferred))
            })
            .or_else(|| ports.first());

        let Some(port) = target else {
            return;
        };

        let port_name = midi_in.port_name(port).ok();
        let tx = self.msg_tx.clone();
        let repaint_context = Arc::clone(&self.repaint_context);
        match midi_in.connect(
            port,
            "tasq-in",
            move |_stamp, raw, _| {
                if let Some(msg) = parse_midi(raw) {
                    if tx.try_send(msg).is_ok() {
                        if let Some(context) = repaint_context.get() {
                            context.request_repaint();
                        }
                    }
                }
            },
            (),
        ) {
            Ok(connection) => {
                self.current_port = port_name;
                self._connection = Some(connection);
                info!(
                    "Connected MIDI Input: {}",
                    self.current_port.as_deref().unwrap_or("unknown")
                );
            }
            Err(e) => warn!("Could not connect MIDI input: {e}"),
        }
    }

    pub fn drain(&self) -> Vec<MidiMessage> {
        let mut out = Vec::new();
        while let Ok(m) = self.msg_rx.try_recv() {
            out.push(m);
        }
        out
    }
}

pub struct MidiOutputManager {
    pub port_names: Vec<String>,
    pub current_port: Option<String>,
    connection: Option<MidiOutputConnection>,
}

impl MidiOutputManager {
    pub fn new() -> Self {
        let mut mgr = Self {
            port_names: Vec::new(),
            current_port: None,
            connection: None,
        };
        mgr.refresh_and_connect(None);
        mgr
    }

    pub fn refresh_and_connect(&mut self, preferred_port: Option<&str>) {
        self.connection = None;
        self.current_port = None;
        let Ok(midi_out) = MidiOutput::new("Theatre Show Cue Output") else {
            self.port_names.clear();
            return;
        };
        let ports = midi_out.ports();
        self.port_names = ports
            .iter()
            .filter_map(|p| midi_out.port_name(p).ok())
            .collect();

        let target_port = if let Some(pref) = preferred_port {
            ports
                .iter()
                .find(|p| midi_out.port_name(p).ok().as_deref() == Some(pref))
                .or_else(|| ports.first())
        } else {
            ports.first()
        };

        if let Some(p) = target_port {
            let pname = midi_out.port_name(p).unwrap_or_default();
            if let Ok(conn) = midi_out.connect(p, "tasq-out") {
                info!("Connected MIDI Output: {}", pname);
                self.current_port = Some(pname);
                self.connection = Some(conn);
            }
        }
    }

    pub fn send_bytes(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if let Some(ref mut conn) = self.connection {
            conn.send(bytes)
                .map_err(|e| anyhow::anyhow!("MIDI send error: {e}"))?;
        } else {
            warn!(
                "No MIDI Output port connected; bytes {:02X?} not sent",
                bytes
            );
        }
        Ok(())
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
