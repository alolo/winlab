// src/midi/input.rs — MIDI вход и MIDI выход для отправки управляющих команд и Raw Hex

use crate::dispatch::DispatchTiming;
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};
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

/// Connection transitions reported by the MIDI Output owner thread.
/// These are state notifications only; they never send MIDI data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MidiOutputConnectionEvent {
    Connected { port_name: String },
    Disconnected { port_name: String },
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
        timing: DispatchTiming,
        queued_at: Instant,
        events: Vec<MidiDispatch>,
    },
    Shutdown,
}

// Polling and reconnection always run on the native MIDI owner thread. This
// preserves strict drop-before-open ordering for WinMM connections.
fn poll_midi_input_recovery(
    connection: &mut Option<MidiInputConnection<()>>,
    desired_port: &mut Option<String>,
    request_id: u64,
    status_tx: &Sender<MidiPortStatus>,
    message_tx: &Sender<MidiMessage>,
    repaint_context: &Arc<OnceLock<egui::Context>>,
) {
    let midi_in = match MidiInput::new("Theatre Show Cue Input") {
        Ok(midi_in) => midi_in,
        Err(error) => {
            if connection.take().is_some() {
                info!(
                    "MIDI input backend became unavailable; releasing the old connection: {error}"
                );
                let _ = status_tx.send(MidiPortStatus {
                    request_id,
                    port_names: Vec::new(),
                    current_port: None,
                    error: Some(format!("MIDI input backend unavailable: {error}")),
                });
            } else {
                debug!("MIDI input recovery enumeration failed: {error}");
            }
            return;
        }
    };
    let ports = midi_in.ports();
    let port_names: Vec<String> = ports
        .iter()
        .filter_map(|port| midi_in.port_name(port).ok())
        .collect();
    if desired_port.is_none() {
        *desired_port = port_names.first().cloned();
    }
    let selected_name = desired_port.clone();
    let target = selected_name.as_deref().and_then(|preferred| {
        ports
            .iter()
            .find(|port| midi_in.port_name(port).ok().as_deref() == Some(preferred))
    });

    let Some(port) = target else {
        if connection.take().is_some() {
            info!(
                "MIDI input disconnected: selected_port='{}'; waiting for the same port to return",
                selected_name.as_deref().unwrap_or("<none>")
            );
            let _ = status_tx.send(MidiPortStatus {
                request_id,
                port_names,
                current_port: None,
                error: None,
            });
        }
        return;
    };

    if connection.is_some() {
        return;
    }

    let port_name = selected_name.unwrap_or_default();
    let callback_tx = message_tx.clone();
    let callback_context = Arc::clone(repaint_context);
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
            info!("MIDI input automatically reconnected: {}", port_name);
            *connection = Some(new_connection);
            let _ = status_tx.send(MidiPortStatus {
                request_id,
                port_names,
                current_port: Some(port_name),
                error: None,
            });
        }
        Err(error) => debug!(
            "MIDI input reconnect attempt failed for '{}': {}",
            port_name, error
        ),
    }
}

/// A single background owner serializes native MIDI input teardown, creation,
/// and recovery without blocking the GUI thread.
fn run_midi_input_worker(
    command_rx: Receiver<MidiInputCommand>,
    status_tx: Sender<MidiPortStatus>,
    message_tx: Sender<MidiMessage>,
    repaint_context: Arc<OnceLock<egui::Context>>,
) {
    let mut connection: Option<MidiInputConnection<()>> = None;
    let mut desired_port: Option<String> = None;
    let mut active_request_id = 0_u64;

    loop {
        let command = match command_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(command) => command,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                poll_midi_input_recovery(
                    &mut connection,
                    &mut desired_port,
                    active_request_id,
                    &status_tx,
                    &message_tx,
                    &repaint_context,
                );
                continue;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        match command {
            MidiInputCommand::Connect {
                request_id,
                preferred_port,
            } => {
                active_request_id = request_id;
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

                desired_port = preferred_port
                    .clone()
                    .or_else(|| port_names.first().cloned());
                let target = desired_port.as_deref().and_then(|preferred| {
                    ports
                        .iter()
                        .find(|port| midi_in.port_name(port).ok().as_deref() == Some(preferred))
                });

                let Some(port) = target else {
                    let error = preferred_port.map(|preferred| {
                        format!(
                            "Selected MIDI input '{}' is unavailable; waiting for that exact port.",
                            preferred
                        )
                    });
                    if let Some(message) = error.as_deref() {
                        warn!("{message}");
                    } else {
                        info!("No MIDI input ports are currently available");
                    }
                    let _ = status_tx.send(MidiPortStatus {
                        request_id,
                        port_names,
                        current_port: None,
                        error,
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

fn poll_midi_output_recovery(
    connection: &mut Option<MidiOutputConnection>,
    connected_port_name: &mut Option<String>,
    desired_port: &mut Option<String>,
    request_id: u64,
    status_tx: &Sender<MidiPortStatus>,
) {
    let midi_out = match MidiOutput::new("Theatre Show Cue Output") {
        Ok(midi_out) => midi_out,
        Err(error) => {
            if connection.take().is_some() {
                info!(
                    "MIDI output backend became unavailable; releasing the old connection: {error}"
                );
                *connected_port_name = None;
                let _ = status_tx.send(MidiPortStatus {
                    request_id,
                    port_names: Vec::new(),
                    current_port: None,
                    error: Some(format!("MIDI output backend unavailable: {error}")),
                });
            } else {
                debug!("MIDI output recovery enumeration failed: {error}");
            }
            return;
        }
    };
    let ports = midi_out.ports();
    let port_names: Vec<String> = ports
        .iter()
        .filter_map(|port| midi_out.port_name(port).ok())
        .collect();
    if desired_port.is_none() {
        *desired_port = port_names.first().cloned();
    }
    let selected_name = desired_port.clone();
    let target = selected_name.as_deref().and_then(|preferred| {
        ports
            .iter()
            .find(|port| midi_out.port_name(port).ok().as_deref() == Some(preferred))
    });

    let Some(port) = target else {
        if connection.take().is_some() {
            info!(
                "MIDI output disconnected: selected_port='{}'; waiting for the same port to return",
                selected_name.as_deref().unwrap_or("<none>")
            );
            *connected_port_name = None;
            let _ = status_tx.send(MidiPortStatus {
                request_id,
                port_names,
                current_port: None,
                error: None,
            });
        }
        return;
    };

    if connection.is_some() {
        return;
    }

    let port_name = selected_name.unwrap_or_default();
    match midi_out.connect(port, "tasq-out") {
        Ok(new_connection) => {
            info!("MIDI output automatically reconnected: {}", port_name);
            *connected_port_name = Some(port_name.clone());
            *connection = Some(new_connection);
            let _ = status_tx.send(MidiPortStatus {
                request_id,
                port_names,
                current_port: Some(port_name),
                error: None,
            });
        }
        Err(error) => debug!(
            "MIDI output reconnect attempt failed for '{}': {}",
            port_name, error
        ),
    }
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
    let mut desired_port: Option<String> = None;
    let mut active_request_id = 0_u64;

    loop {
        let command = match command_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(command) => command,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                poll_midi_output_recovery(
                    &mut connection,
                    &mut connected_port_name,
                    &mut desired_port,
                    active_request_id,
                    &status_tx,
                );
                continue;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        match command {
            MidiOutputCommand::Connect {
                request_id,
                preferred_port,
            } => {
                active_request_id = request_id;
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

                desired_port = preferred_port
                    .clone()
                    .or_else(|| port_names.first().cloned());
                let target = desired_port.as_deref().and_then(|preferred| {
                    ports
                        .iter()
                        .find(|port| midi_out.port_name(port).ok().as_deref() == Some(preferred))
                });

                let Some(port) = target else {
                    let error = preferred_port.map(|preferred| {
                        format!(
                        "Selected MIDI output '{}' is unavailable; waiting for that exact port.",
                        preferred
                    )
                    });
                    if let Some(message) = error.as_deref() {
                        warn!("{message}");
                    } else {
                        info!("No MIDI output ports are currently available");
                    }
                    let _ = status_tx.send(MidiPortStatus {
                        request_id,
                        port_names,
                        current_port: None,
                        error,
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
                let mut connection_failed = false;
                match connection.as_mut() {
                    Some(connection) => match connection.send(&bytes) {
                        Ok(()) => info!(
                            "MIDI message send accepted: port='{}', bytes={:02X?}, send_call_us={}",
                            connected_port_name.as_deref().unwrap_or("unknown"),
                            bytes,
                            started_at.elapsed().as_micros()
                        ),
                        Err(error) => {
                            connection_failed = true;
                            warn!(
                                "MIDI output send failed: port='{}', bytes={:02X?}, error={}; releasing connection for exact-port recovery",
                                connected_port_name.as_deref().unwrap_or("unknown"),
                                bytes,
                                error
                            );
                        }
                    },
                    None => warn!(
                        "No MIDI Output port connected; bytes {:02X?} not sent",
                        bytes
                    ),
                }
                if connection_failed {
                    drop(connection.take());
                    connected_port_name = None;
                    let _ = status_tx.send(MidiPortStatus {
                        request_id: active_request_id,
                        port_names: Vec::new(),
                        current_port: None,
                        error: Some(
                            "MIDI output send failed; waiting to reconnect the selected port"
                                .to_string(),
                        ),
                    });
                }
            }
            MidiOutputCommand::SendBatch {
                transaction_id,
                timing,
                queued_at,
                events,
            } => {
                let worker_started_at = Instant::now();
                let queue_wait_us = worker_started_at
                    .saturating_duration_since(queued_at)
                    .as_micros();
                let port_name = connected_port_name
                    .clone()
                    .unwrap_or_else(|| "unconnected".to_string());
                let mut sent = 0usize;
                let mut failed = 0usize;
                let mut skipped = 0usize;
                let mut results = Vec::with_capacity(events.len());
                let mut first_send_started_at: Option<Instant> = None;
                let mut first_send_completed_at: Option<Instant> = None;
                let mut last_send_completed_at: Option<Instant> = None;
                let mut first_send_call_us: Option<u128> = None;
                let mut last_send_call_us: Option<u128> = None;
                let mut first_send_cue_id: Option<Uuid> = None;
                let mut last_send_cue_id: Option<Uuid> = None;
                let mut native_connection_failed = false;

                for event in events {
                    let batch_offset_us = worker_started_at.elapsed().as_micros();
                    let MidiDispatch {
                        cue_id,
                        cue_number,
                        cue_name,
                        bytes,
                    } = event;

                    if bytes.is_empty() {
                        skipped += 1;
                        results.push(MidiSendResult {
                            cue_id,
                            cue_number,
                            cue_name,
                            bytes,
                            status: "skipped_empty_payload".to_string(),
                            batch_offset_us,
                            send_call_us: 0,
                        });
                        continue;
                    }

                    match connection.as_mut() {
                        Some(connection) => {
                            // Capture timestamps immediately around the native API call.
                            // Diagnostic logging is deliberately deferred until the batch
                            // has finished so file I/O cannot delay the first MIDI send.
                            let send_started_at = Instant::now();
                            if first_send_started_at.is_none() {
                                first_send_started_at = Some(send_started_at);
                                first_send_cue_id = Some(cue_id);
                            }

                            let send_result = connection.send(&bytes);
                            let send_completed_at = Instant::now();
                            let send_call_us = send_completed_at
                                .saturating_duration_since(send_started_at)
                                .as_micros();
                            if first_send_completed_at.is_none() {
                                first_send_completed_at = Some(send_completed_at);
                                first_send_call_us = Some(send_call_us);
                            }
                            last_send_completed_at = Some(send_completed_at);
                            last_send_call_us = Some(send_call_us);
                            last_send_cue_id = Some(cue_id);

                            match send_result {
                                Ok(()) => {
                                    sent += 1;
                                    results.push(MidiSendResult {
                                        cue_id,
                                        cue_number,
                                        cue_name,
                                        bytes,
                                        status: "backend_accepted".to_string(),
                                        batch_offset_us,
                                        send_call_us,
                                    });
                                }
                                Err(error) => {
                                    failed += 1;
                                    native_connection_failed = true;
                                    results.push(MidiSendResult {
                                        cue_id,
                                        cue_number,
                                        cue_name,
                                        bytes,
                                        status: format!("send_failed: {error}"),
                                        batch_offset_us,
                                        send_call_us,
                                    });
                                }
                            }
                        }
                        None => {
                            failed += 1;
                            results.push(MidiSendResult {
                                cue_id,
                                cue_number,
                                cue_name,
                                bytes,
                                status: "no_output_connected".to_string(),
                                batch_offset_us,
                                send_call_us: 0,
                            });
                        }
                    }
                }

                let worker_completed_at = Instant::now();
                let worker_elapsed_us = worker_completed_at
                    .saturating_duration_since(worker_started_at)
                    .as_micros();
                crate::performance::record_midi_batch(
                    results.len(),
                    sent,
                    failed,
                    skipped,
                    queue_wait_us,
                    worker_elapsed_us,
                );
                let first_to_last_send_span_us =
                    match (first_send_started_at, last_send_completed_at) {
                        (Some(first), Some(last)) => {
                            Some(last.saturating_duration_since(first).as_micros())
                        }
                        _ => None,
                    };

                // Emit per-Cue results only after all native calls have completed.
                // A successful midir call means the backend accepted the message;
                // it is not an acknowledgement from the receiving hardware.
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

                let go_to_worker_started_us = timing.go_elapsed_us(worker_started_at);
                let go_to_first_send_started_us =
                    first_send_started_at.and_then(|at| timing.go_elapsed_us(at));
                let go_to_first_send_completed_us =
                    first_send_completed_at.and_then(|at| timing.go_elapsed_us(at));
                let go_to_last_send_completed_us =
                    last_send_completed_at.and_then(|at| timing.go_elapsed_us(at));
                let transaction_to_worker_started_us =
                    timing.transaction_elapsed_us(worker_started_at);
                let transaction_to_first_send_started_us =
                    first_send_started_at.map(|at| timing.transaction_elapsed_us(at));
                let transaction_to_first_send_completed_us =
                    first_send_completed_at.map(|at| timing.transaction_elapsed_us(at));
                let transaction_to_last_send_completed_us =
                    last_send_completed_at.map(|at| timing.transaction_elapsed_us(at));
                let worker_to_first_send_started_us = first_send_started_at
                    .map(|at| at.saturating_duration_since(worker_started_at).as_micros());
                let worker_to_first_send_completed_us = first_send_completed_at
                    .map(|at| at.saturating_duration_since(worker_started_at).as_micros());
                let worker_to_last_send_completed_us = last_send_completed_at
                    .map(|at| at.saturating_duration_since(worker_started_at).as_micros());

                if native_connection_failed {
                    drop(connection.take());
                    connected_port_name = None;
                    let _ = status_tx.send(MidiPortStatus {
                        request_id: active_request_id,
                        port_names: Vec::new(),
                        current_port: None,
                        error: Some(
                            "MIDI output batch send failed; waiting to reconnect the selected port"
                                .to_string(),
                        ),
                    });
                }

                info!(
                    target: "TaSQ::midi::input",
                    "MIDI dispatch latency: transaction_id={}, stage=LATENCY_SUMMARY, events={}, sent={}, failed={}, skipped={}, go_to_transaction_started_us={:?}, transaction_started_to_go_received_us={:?}, go_to_batch_queued_us={:?}, go_to_worker_started_us={:?}, go_to_first_send_started_us={:?}, go_to_first_send_completed_us={:?}, go_to_last_send_completed_us={:?}, transaction_to_batch_queued_us={}, batch_queued_to_worker_started_us={}, transaction_to_worker_started_us={}, transaction_to_first_send_started_us={:?}, transaction_to_first_send_completed_us={:?}, transaction_to_last_send_completed_us={:?}, queue_wait_us={}, worker_to_first_send_started_us={:?}, worker_to_first_send_completed_us={:?}, worker_to_last_send_completed_us={:?}, worker_elapsed_us={}, first_send_call_us={:?}, last_send_call_us={:?}, first_to_last_send_span_us={:?}, first_send_cue_id={:?}, last_send_cue_id={:?}, go_origin_present={}",
                    transaction_id,
                    results.len(),
                    sent,
                    failed,
                    skipped,
                    timing.go_elapsed_us(timing.transaction_started_at),
                    timing.transaction_to_go_received_us(),
                    timing.go_elapsed_us(queued_at),
                    go_to_worker_started_us,
                    go_to_first_send_started_us,
                    go_to_first_send_completed_us,
                    go_to_last_send_completed_us,
                    timing.transaction_elapsed_us(queued_at),
                    queue_wait_us,
                    transaction_to_worker_started_us,
                    transaction_to_first_send_started_us,
                    transaction_to_first_send_completed_us,
                    transaction_to_last_send_completed_us,
                    queue_wait_us,
                    worker_to_first_send_started_us,
                    worker_to_first_send_completed_us,
                    worker_to_last_send_completed_us,
                    worker_elapsed_us,
                    first_send_call_us,
                    last_send_call_us,
                    first_to_last_send_span_us,
                    first_send_cue_id,
                    last_send_cue_id,
                    timing.go_received_at.is_some()
                );

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

fn midi_output_connection_event(
    previous_port: Option<&str>,
    current_port: Option<&str>,
) -> Option<MidiOutputConnectionEvent> {
    match (previous_port, current_port) {
        (Some(previous), None) => Some(MidiOutputConnectionEvent::Disconnected {
            port_name: previous.to_owned(),
        }),
        (None, Some(current)) => Some(MidiOutputConnectionEvent::Connected {
            port_name: current.to_owned(),
        }),
        (Some(previous), Some(current)) if previous != current => {
            Some(MidiOutputConnectionEvent::Connected {
                port_name: current.to_owned(),
            })
        }
        _ => None,
    }
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

    /// Apply native MIDI status updates and report connection transitions.
    /// The one-second owner-thread polling remains responsible for detecting
    /// when the selected port disappears from the backend's device list.
    pub fn poll(&mut self) -> Vec<MidiOutputConnectionEvent> {
        let mut transitions = Vec::new();

        while let Ok(status) = self.status_rx.try_recv() {
            if status.request_id != self.latest_request_id {
                continue;
            }

            if let Some(transition) = midi_output_connection_event(
                self.current_port.as_deref(),
                status.current_port.as_deref(),
            ) {
                transitions.push(transition);
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

        transitions
    }

    pub fn send_batch(
        &mut self,
        transaction_id: Uuid,
        timing: DispatchTiming,
        events: Vec<MidiDispatch>,
    ) -> anyhow::Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let event_count = events.len();
        // Timestamp immediately before enqueue. The worker uses this same
        // monotonic instant to calculate queue wait without wall-clock drift.
        let queued_at = Instant::now();
        self.command_tx
            .send(MidiOutputCommand::SendBatch {
                transaction_id,
                timing,
                queued_at,
                events,
            })
            .map_err(|error| anyhow::anyhow!("Could not queue MIDI dispatch batch: {error}"))?;
        info!(
            target: "TaSQ::midi::input",
            "MIDI dispatch stage: transaction_id={} stage=BATCH_QUEUED events={} go_to_batch_queued_us={:?} transaction_to_batch_queued_us={} enqueue_call_us={}",
            transaction_id,
            event_count,
            timing.go_elapsed_us(queued_at),
            timing.transaction_elapsed_us(queued_at),
            queued_at.elapsed().as_micros()
        );
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
    use super::{
        midi_output_connection_event, MidiInputCommand, MidiOutputCommand,
        MidiOutputConnectionEvent,
    };
    use crate::dispatch::DispatchTiming;
    use crossbeam_channel::unbounded;

    #[test]
    fn output_connection_transition_reports_first_available_port() {
        assert_eq!(
            midi_output_connection_event(None, Some("MIDI Control 1")),
            Some(MidiOutputConnectionEvent::Connected {
                port_name: "MIDI Control 1".to_string(),
            })
        );
    }

    #[test]
    fn output_connection_transition_ignores_unchanged_port() {
        assert_eq!(
            midi_output_connection_event(Some("MIDI Control 1"), Some("MIDI Control 1")),
            None
        );
    }

    #[test]
    fn output_connection_transition_reports_changed_port() {
        assert_eq!(
            midi_output_connection_event(Some("MIDI Control 1"), Some("MIDI Control 2")),
            Some(MidiOutputConnectionEvent::Connected {
                port_name: "MIDI Control 2".to_string(),
            })
        );
    }

    #[test]
    fn output_disconnection_reports_the_previously_connected_port() {
        assert_eq!(
            midi_output_connection_event(Some("MIDI Control 1"), None),
            Some(MidiOutputConnectionEvent::Disconnected {
                port_name: "MIDI Control 1".to_string(),
            })
        );
    }

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

        let transaction_started_at = Instant::now();
        tx.send(MidiOutputCommand::SendBatch {
            transaction_id,
            timing: DispatchTiming {
                go_received_at: Some(transaction_started_at),
                transaction_started_at,
            },
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
