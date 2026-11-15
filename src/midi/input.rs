// src/midi/input.rs — MIDI вход и MIDI выход для отправки управляющих команд и Raw Hex

use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Instant;
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MidiMessage {
    NoteOn { channel: u8, note: u8, velocity: u8 },
    NoteOff { channel: u8, note: u8 },
    CC { channel: u8, cc: u8, value: u8 },
    SysEx(Vec<u8>),
}

#[derive(Debug)]
enum MidiInputCommand {
    Connect {
        request_id: u64,
        preferred_port: Option<String>,
    },
    Shutdown,
}

#[derive(Debug)]
struct MidiPortStatus {
    request_id: u64,
    port_names: Vec<String>,
    current_port: Option<String>,
    error: Option<String>,
}

/// One Cue's MIDI payload inside an application-level launch transaction.
#[derive(Debug, Clone)]
pub struct MidiDispatch {
    pub cue_id: Uuid,
    pub cue_number: String,
    pub cue_name: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
struct MidiSendResult {
    cue_id: Uuid,
    cue_number: String,
    cue_name: String,
    bytes: Vec<u8>,
    status: String,
    batch_offset_us: u128,
    send_call_us: u128,
}

#[derive(Debug)]
enum MidiOutputCommand {
    Connect {
        request_id: u64,
        preferred_port: Option<String>,
    },
    Send(Vec<u8>),
    SendBatch {
        transaction_id: Uuid,
        queued_at: Instant,
        events: Vec<MidiDispatch>,
    },
    Shutdown,
}

/// A single background owner serializes native MIDI input teardown and creation.
///
/// WinMM can report MMSYSERR_ALLOCATED if a replacement connection is opened
/// before the previous MidiInputConnection has completed its native teardown.
/// Keeping the connection on one worker thread guarantees strict drop-before-open
/// ordering without blocking the GUI thread.
fn run_midi_input_worker(
    command_rx: Receiver<MidiInputCommand>,
    status_tx: Sender<MidiPortStatus>,
    message_tx: Sender<MidiMessage>,
    repaint_context: Arc<OnceLock<egui::Context>>,
) {
    let mut connection: Option<MidiInputConnection<()>> = None;

    while let Ok(command) = command_rx.recv() {
        match command {
            MidiInputCommand::Connect {
                request_id,
                preferred_port,
            } => {
                if let Some(previous) = connection.take() {
                    info!("Releasing previous native MIDI input connection before opening a replacement");
                    // The connection is dropped here, on this worker thread. The
                    // next open cannot begin until WinMM teardown has returned.
                    drop(previous);
                }

                let midi_in = match MidiInput::new("Theatre Show Cue Input") {
                    Ok(midi_in) => midi_in,
                    Err(error) => {
                        let message = format!("MIDI input initialization failed: {error}");
                        warn!("{message}");
                        let _ = status_tx.send(MidiPortStatus {
                            request_id,
                            port_names: Vec::new(),
                            current_port: None,
                            error: Some(message),
                        });
                        continue;
                    }
                };

                let ports = midi_in.ports();
                let port_names: Vec<String> = ports
                    .iter()
                    .filter_map(|port| midi_in.port_name(port).ok())
                    .collect();

                let target = preferred_port
                    .as_deref()
                    .and_then(|preferred| {
                        ports
                            .iter()
                            .find(|port| midi_in.port_name(port).ok().as_deref() == Some(preferred))
                    })
                    .or_else(|| ports.first());

                let Some(port) = target else {
                    info!("No MIDI input ports are currently available");
                    let _ = status_tx.send(MidiPortStatus {
                        request_id,
                        port_names,
                        current_port: None,
                        error: None,
                    });
                    continue;
                };

                let port_name = midi_in.port_name(port).ok();
                let callback_tx = message_tx.clone();
                let callback_context = Arc::clone(&repaint_context);
                match midi_in.connect(
                    port,
                    "tasq-in",
                    move |_stamp, raw, _| {
                        if let Some(message) = parse_midi(raw) {
                            if callback_tx.try_send(message).is_ok() {
                                if let Some(context) = callback_context.get() {
                                    context.request_repaint();
                                }
                            }
                        }
                    },
                    (),
                ) {
                    Ok(new_connection) => {
                        info!(
                            "Connected MIDI Input: {}",
                            port_name.as_deref().unwrap_or("unknown")
                        );
                        connection = Some(new_connection);
                        let _ = status_tx.send(MidiPortStatus {
                            request_id,
                            port_names,
                            current_port: port_name,
                            error: None,
                        });
                    }
                    Err(error) => {
                        let message = format!("Could not connect MIDI input: {error}");
                        warn!("{message}");
                        let _ = status_tx.send(MidiPortStatus {
                            request_id,
                            port_names,
                            current_port: None,
                            error: Some(message),
                        });
                    }
                }
            }
            MidiInputCommand::Shutdown => {
                info!("MIDI input worker shutting down; releasing native connection");
                drop(connection.take());
                break;
            }
        }
    }

    info!("MIDI input worker stopped");
}

/// A single background owner serializes native MIDI output teardown, creation,
/// and sends. This also prevents a cue from sending to a connection while it is
/// being replaced by the operator in MIDI settings.
fn run_midi_output_worker(
    command_rx: Receiver<MidiOutputCommand>,
    status_tx: Sender<MidiPortStatus>,
) {
    let mut connection: Option<MidiOutputConnection> = None;
    let mut connected_port_name: Option<String> = None;

    while let Ok(command) = command_rx.recv() {
        match command {
            MidiOutputCommand::Connect {
                request_id,
                preferred_port,
            } => {
                if let Some(previous) = connection.take() {
                    info!("Releasing previous native MIDI output connection before opening a replacement");
                    drop(previous);
                }
                connected_port_name = None;

                let midi_out = match MidiOutput::new("Theatre Show Cue Output") {
                    Ok(midi_out) => midi_out,
                    Err(error) => {
                        let message = format!("MIDI output initialization failed: {error}");
                        warn!("{message}");
                        let _ = status_tx.send(MidiPortStatus {
                            request_id,
                            port_names: Vec::new(),
                            current_port: None,
                            error: Some(message),
                        });
                        continue;
                    }
                };

                let ports = midi_out.ports();
                let port_names: Vec<String> = ports
                    .iter()
                    .filter_map(|port| midi_out.port_name(port).ok())
                    .collect();
                info!("Enumerated MIDI output ports: {:?}", port_names);

                let preferred_target = preferred_port.as_deref().and_then(|preferred| {
                    ports
                        .iter()
                        .find(|port| midi_out.port_name(port).ok().as_deref() == Some(preferred))
                });
                let preferred_found = preferred_target.is_some();
                let target = preferred_target.or_else(|| ports.first());

                if let Some(preferred) = preferred_port.as_deref() {
                    if !preferred_found {
                        warn!(
                            "Preferred MIDI output '{}' was not found; falling back to the first available output port",
                            preferred
                        );
                    }
                }

                let Some(port) = target else {
                    info!("No MIDI output ports are currently available");
                    let _ = status_tx.send(MidiPortStatus {
                        request_id,
                        port_names,
                        current_port: None,
                        error: None,
                    });
                    continue;
                };

                let port_name = midi_out.port_name(port).unwrap_or_default();
                match midi_out.connect(port, "tasq-out") {
                    Ok(new_connection) => {
                        info!("Connected MIDI Output: {}", port_name);
                        connected_port_name = Some(port_name.clone());
                        connection = Some(new_connection);
                        let _ = status_tx.send(MidiPortStatus {
                            request_id,
                            port_names,
                            current_port: Some(port_name),
                            error: None,
                        });
                    }
                    Err(error) => {
                        let message = format!("Could not connect MIDI output: {error}");
                        warn!("{message}");
                        let _ = status_tx.send(MidiPortStatus {
                            request_id,
                            port_names,
                            current_port: None,
                            error: Some(message),
                        });
                    }
                }
            }
            MidiOutputCommand::Send(bytes) => {
                if bytes.is_empty() {
                    continue;
                }
                let started_at = Instant::now();
                match connection.as_mut() {
                    Some(connection) => match connection.send(&bytes) {
                        Ok(()) => info!(
                            "MIDI message send accepted: port='{}', bytes={:02X?}, send_call_us={}",
                            connected_port_name.as_deref().unwrap_or("unknown"),
                            bytes,
                            started_at.elapsed().as_micros()
                        ),
                        Err(error) => warn!(
                            "MIDI output send failed: port='{}', bytes={:02X?}, error={}",
                            connected_port_name.as_deref().unwrap_or("unknown"),
                            bytes,
                            error
                        ),
                    },
                    None => warn!(
                        "No MIDI Output port connected; bytes {:02X?} not sent",
                        bytes
                    ),
                }
            }
            MidiOutputCommand::SendBatch {
                transaction_id,
                queued_at,
                events,
            } => {
                let queue_wait_us = queued_at.elapsed().as_micros();
                let port_name = connected_port_name.as_deref().unwrap_or("unconnected");
                let batch_started_at = Instant::now();
                let mut sent = 0usize;
                let mut failed = 0usize;
                let mut skipped = 0usize;
                let mut results = Vec::with_capacity(events.len());

                for event in events {
                    let batch_offset_us = batch_started_at.elapsed().as_micros();
                    if event.bytes.is_empty() {
                        skipped += 1;
                        results.push(MidiSendResult {
                            cue_id: event.cue_id,
                            cue_number: event.cue_number,
                            cue_name: event.cue_name,
                            bytes: event.bytes,
                            status: "skipped_empty_payload".to_string(),
                            batch_offset_us,
                            send_call_us: 0,
                        });
                        continue;
                    }

                    let send_started_at = Instant::now();
                    match connection.as_mut() {
                        Some(connection) => match connection.send(&event.bytes) {
                            Ok(()) => {
                                sent += 1;
                                results.push(MidiSendResult {
                                    cue_id: event.cue_id,
                                    cue_number: event.cue_number,
                                    cue_name: event.cue_name,
                                    bytes: event.bytes,
                                    status: "backend_accepted".to_string(),
                                    batch_offset_us,
                                    send_call_us: send_started_at.elapsed().as_micros(),
                                });
                            }
                            Err(error) => {
                                failed += 1;
                                results.push(MidiSendResult {
                                    cue_id: event.cue_id,
                                    cue_number: event.cue_number,
                                    cue_name: event.cue_name,
                                    bytes: event.bytes,
                                    status: format!("send_failed: {error}"),
                                    batch_offset_us,
                                    send_call_us: send_started_at.elapsed().as_micros(),
                                });
                            }
                        },
                        None => {
                            failed += 1;
                            results.push(MidiSendResult {
                                cue_id: event.cue_id,
                                cue_number: event.cue_number,
                                cue_name: event.cue_name,
                                bytes: event.bytes,
                                status: "no_output_connected".to_string(),
                                batch_offset_us,
                                send_call_us: 0,
                            });
                        }
                    }
                }

                let worker_elapsed_us = batch_started_at.elapsed().as_micros();

                // Emit one structured diagnostic per Cue. A successful backend
                // call means the MIDI API accepted the message; it is not an
                // acknowledgement from the receiving hardware.
                for result in &results {
                    if result.status == "backend_accepted" {
                        info!(
                            "MIDI Cue send result: transaction_id={}, cue_id={}, cue_number='{}', cue_name='{}', port='{}', bytes={:02X?}, status='{}', batch_offset_us={}, send_call_us={}",
                            transaction_id,
                            result.cue_id,
                            &result.cue_number,
                            &result.cue_name,
                            port_name,
                            &result.bytes,
                            &result.status,
                            result.batch_offset_us,
                            result.send_call_us
                        );
                    } else {
                        warn!(
                            "MIDI Cue send result: transaction_id={}, cue_id={}, cue_number='{}', cue_name='{}', port='{}', bytes={:02X?}, status='{}', batch_offset_us={}, send_call_us={}",
                            transaction_id,
                            result.cue_id,
                            &result.cue_number,
                            &result.cue_name,
                            port_name,
                            &result.bytes,
                            &result.status,
                            result.batch_offset_us,
                            result.send_call_us
                        );
                    }
                }

                if failed > 0 {
                    warn!(
                        "MIDI dispatch batch completed with errors: transaction_id={}, port='{}', events={}, sent={}, failed={}, skipped={}, queue_wait_us={}, worker_elapsed_us={}",
                        transaction_id,
                        port_name,
                        results.len(),
                        sent,
                        failed,
                        skipped,
                        queue_wait_us,
                        worker_elapsed_us
                    );
                } else {
                    info!(
                        "MIDI dispatch batch completed: transaction_id={}, port='{}', events={}, sent={}, failed={}, skipped={}, queue_wait_us={}, worker_elapsed_us={}",
                        transaction_id,
                        port_name,
                        results.len(),
                        sent,
                        failed,
                        skipped,
                        queue_wait_us,
                        worker_elapsed_us
                    );
                }
            }
            MidiOutputCommand::Shutdown => {
                info!("MIDI output worker shutting down; releasing native connection");
                drop(connection.take());
                break;
            }
        }
    }

    info!("MIDI output worker stopped");
}

pub struct MidiInputManager {
    pub msg_rx: Receiver<MidiMessage>,
    pub port_names: Vec<String>,
    pub current_port: Option<String>,
    command_tx: Sender<MidiInputCommand>,
    status_rx: Receiver<MidiPortStatus>,
    latest_request_id: u64,
    repaint_context: Arc<OnceLock<egui::Context>>,
}

impl MidiInputManager {
    pub fn new_with_preferred(preferred_port: Option<&str>) -> Self {
        let (message_tx, msg_rx) = bounded::<MidiMessage>(2048);
        let (command_tx, command_rx) = unbounded::<MidiInputCommand>();
        let (status_tx, status_rx) = unbounded::<MidiPortStatus>();
        let repaint_context = Arc::new(OnceLock::new());

        let worker_context = Arc::clone(&repaint_context);
        match thread::Builder::new()
            .name("tasq-midi-input-owner".to_string())
            .spawn(move || {
                run_midi_input_worker(command_rx, status_tx, message_tx, worker_context);
            }) {
            Ok(handle) => drop(handle),
            Err(error) => error!("Could not start MIDI input owner thread: {error}"),
        }

        let mut manager = Self {
            msg_rx,
            port_names: Vec::new(),
            current_port: None,
            command_tx,
            status_rx,
            latest_request_id: 0,
            repaint_context,
        };
        manager.refresh_and_connect(preferred_port);
        manager
    }

    /// Attach the UI context so MIDI input can wake an otherwise idle egui app.
    /// The native MIDI callback only enqueues the message and requests a repaint;
    /// all application state changes still happen on the UI thread.
    pub fn set_repaint_context(&mut self, context: egui::Context) {
        let _ = self.repaint_context.set(context);
    }

    pub fn refresh_and_connect(&mut self, preferred_port: Option<&str>) {
        self.latest_request_id = self.latest_request_id.wrapping_add(1).max(1);
        let request_id = self.latest_request_id;
        let command = MidiInputCommand::Connect {
            request_id,
            preferred_port: preferred_port.map(str::to_owned),
        };
        if let Err(error) = self.command_tx.send(command) {
            warn!("Could not queue MIDI input connection request: {error}");
        }
    }

    fn poll_connection_status(&mut self) {
        while let Ok(status) = self.status_rx.try_recv() {
            if status.request_id != self.latest_request_id {
                continue;
            }
            self.port_names = status.port_names;
            self.current_port = status.current_port;
            if let Some(error) = status.error {
                warn!("{error}");
            }
        }
    }

    pub fn drain(&mut self) -> Vec<MidiMessage> {
        self.poll_connection_status();
        let mut out = Vec::new();
        while let Ok(message) = self.msg_rx.try_recv() {
            out.push(message);
        }
        out
    }
}

pub struct MidiOutputManager {
    pub port_names: Vec<String>,
    pub current_port: Option<String>,
    command_tx: Sender<MidiOutputCommand>,
    status_rx: Receiver<MidiPortStatus>,
    latest_request_id: u64,
    repaint_context: Arc<OnceLock<egui::Context>>,
}

impl MidiOutputManager {
    pub fn new_with_preferred(preferred_port: Option<&str>) -> Self {
        let (command_tx, command_rx) = unbounded::<MidiOutputCommand>();
        let (status_tx, status_rx) = unbounded::<MidiPortStatus>();
        let repaint_context = Arc::new(OnceLock::new());

        match thread::Builder::new()
            .name("tasq-midi-output-owner".to_string())
            .spawn(move || {
                run_midi_output_worker(command_rx, status_tx);
            }) {
            Ok(handle) => drop(handle),
            Err(error) => error!("Could not start MIDI output owner thread: {error}"),
        }

        let mut manager = Self {
            port_names: Vec::new(),
            current_port: None,
            command_tx,
            status_rx,
            latest_request_id: 0,
            repaint_context,
        };
        manager.refresh_and_connect(preferred_port);
        manager
    }

    pub fn set_repaint_context(&mut self, context: egui::Context) {
        let _ = self.repaint_context.set(context);
    }

    pub fn refresh_and_connect(&mut self, preferred_port: Option<&str>) {
        self.latest_request_id = self.latest_request_id.wrapping_add(1).max(1);
        let request_id = self.latest_request_id;
        if let Err(error) = self.command_tx.send(MidiOutputCommand::Connect {
            request_id,
            preferred_port: preferred_port.map(str::to_owned),
        }) {
            warn!("Could not queue MIDI output connection request: {error}");
        }
    }

    pub fn poll(&mut self) {
        while let Ok(status) = self.status_rx.try_recv() {
            if status.request_id != self.latest_request_id {
                continue;
            }
            self.port_names = status.port_names;
            self.current_port = status.current_port;
            if let Some(error) = status.error {
                warn!("{error}");
            }
            if let Some(context) = self.repaint_context.get() {
                context.request_repaint();
            }
        }
    }

    pub fn send_batch(
        &mut self,
        transaction_id: Uuid,
        events: Vec<MidiDispatch>,
    ) -> anyhow::Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        self.command_tx
            .send(MidiOutputCommand::SendBatch {
                transaction_id,
                queued_at: Instant::now(),
                events,
            })
            .map_err(|error| anyhow::anyhow!("Could not queue MIDI dispatch batch: {error}"))?;
        Ok(())
    }

    pub fn send_bytes(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.command_tx
            .send(MidiOutputCommand::Send(bytes.to_vec()))
            .map_err(|error| anyhow::anyhow!("Could not queue MIDI output bytes: {error}"))
    }
}

impl Drop for MidiInputManager {
    fn drop(&mut self) {
        let _ = self.command_tx.try_send(MidiInputCommand::Shutdown);
    }
}

impl Drop for MidiOutputManager {
    fn drop(&mut self) {
        let _ = self.command_tx.try_send(MidiOutputCommand::Shutdown);
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

#[cfg(test)]
mod midi_worker_serialization_tests {
    use super::{MidiInputCommand, MidiOutputCommand};
    use crossbeam_channel::unbounded;

    #[test]
    fn midi_input_connection_requests_are_queued_in_order() {
        let (tx, rx) = unbounded();
        tx.send(MidiInputCommand::Connect {
            request_id: 1,
            preferred_port: Some("nanoKONTROL2".to_string()),
        })
        .unwrap();
        tx.send(MidiInputCommand::Connect {
            request_id: 2,
            preferred_port: Some("MIDI Port 2".to_string()),
        })
        .unwrap();

        let first = rx.recv().unwrap();
        let second = rx.recv().unwrap();
        assert!(matches!(
            first,
            MidiInputCommand::Connect { request_id: 1, .. }
        ));
        assert!(matches!(
            second,
            MidiInputCommand::Connect { request_id: 2, .. }
        ));
    }

    #[test]
    fn midi_output_sends_are_ordered_after_reconnect_request() {
        let (tx, rx) = unbounded();
        tx.send(MidiOutputCommand::Connect {
            request_id: 7,
            preferred_port: Some("nanoKONTROL2".to_string()),
        })
        .unwrap();
        tx.send(MidiOutputCommand::Send(vec![0x90, 60, 100]))
            .unwrap();

        assert!(matches!(
            rx.recv().unwrap(),
            MidiOutputCommand::Connect { request_id: 7, .. }
        ));
        assert!(matches!(
            rx.recv().unwrap(),
            MidiOutputCommand::Send(bytes) if bytes == vec![0x90, 60, 100]
        ));
    }

    #[test]
    fn midi_dispatch_batch_preserves_cue_order_and_transaction_id() {
        use super::MidiDispatch;
        use std::time::Instant;
        use uuid::Uuid;

        let (tx, rx) = unbounded();
        let transaction_id = Uuid::new_v4();
        let first_id = Uuid::new_v4();
        let second_id = Uuid::new_v4();
        let events = vec![
            MidiDispatch {
                cue_id: first_id,
                cue_number: "1".to_string(),
                cue_name: "First".to_string(),
                bytes: vec![0x90, 60, 100],
            },
            MidiDispatch {
                cue_id: second_id,
                cue_number: "2".to_string(),
                cue_name: "Second".to_string(),
                bytes: vec![0xB0, 7, 100],
            },
        ];

        tx.send(MidiOutputCommand::SendBatch {
            transaction_id,
            queued_at: Instant::now(),
            events,
        })
        .unwrap();

        match rx.recv().unwrap() {
            MidiOutputCommand::SendBatch {
                transaction_id: actual_id,
                events,
                ..
            } => {
                assert_eq!(actual_id, transaction_id);
                assert_eq!(events.len(), 2);
                assert_eq!(events[0].cue_id, first_id);
                assert_eq!(events[1].cue_id, second_id);
                assert_eq!(events[0].bytes, vec![0x90, 60, 100]);
                assert_eq!(events[1].bytes, vec![0xB0, 7, 100]);
            }
            other => panic!("Expected MIDI SendBatch, got {other:?}"),
        }
    }
}
