// src/osc/server.rs — OSC server and batched OSC output dispatch.

use crate::dispatch::DispatchTiming;
use anyhow::Result;
use crossbeam_channel::{bounded, Receiver, Sender};
use rosc::{OscMessage, OscPacket, OscType};
use std::net::{SocketAddr, UdpSocket};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct OscEvent {
    pub addr: String,
    pub args: Vec<OscType>,
}

/// One Cue's OSC payload inside an application-level launch transaction.
#[derive(Debug, Clone)]
pub struct OscDispatch {
    pub cue_id: Uuid,
    pub cue_number: String,
    pub cue_name: String,
    pub addr: String,
    pub args: Vec<OscType>,
}

#[derive(Debug)]
struct OscSendResult {
    cue_id: Uuid,
    cue_number: String,
    cue_name: String,
    address: String,
    args: Vec<OscType>,
    status: String,
    bytes_sent: usize,
    batch_offset_us: u128,
    send_call_us: u128,
}

#[derive(Debug)]
enum OscCommand {
    SendBatch {
        transaction_id: Uuid,
        timing: DispatchTiming,
        queued_at: Instant,
        events: Vec<OscDispatch>,
    },
}

#[allow(dead_code)]
pub struct OscServer {
    pub event_rx: Receiver<OscEvent>,
    command_tx: Sender<OscCommand>,
    pub listen_port: u16,
    pub send_addr: String,
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl OscServer {
    pub fn new(listen_port: u16, send_addr: String) -> Result<Self> {
        let (event_tx, event_rx) = bounded::<OscEvent>(256);
        let (command_tx, send_rx) = bounded::<OscCommand>(256);
        let stop = Arc::new(AtomicBool::new(false));

        let listen_addr = format!("0.0.0.0:{listen_port}");
        let socket = UdpSocket::bind(&listen_addr)?;
        socket.set_nonblocking(true)?;
        info!("OSC listening on {listen_addr}");

        let send_addr_clone = send_addr.clone();
        let mut threads = Vec::new();

        {
            let socket_recv = socket.try_clone()?;
            let tx = event_tx.clone();
            let stop_recv = stop.clone();
            threads.push(
                std::thread::Builder::new()
                    .name("tasq-osc-receiver".to_string())
                    .spawn(move || {
                        let mut buf = [0u8; 8192];
                        while !stop_recv.load(Ordering::Relaxed) {
                            match socket_recv.recv_from(&mut buf) {
                                Ok((size, source)) => {
                                    if let Ok(packet) = rosc::decoder::decode_udp(&buf[..size]) {
                                        flatten_packet(packet.1, &tx, source);
                                    } else {
                                        warn!(
                                            "OSC packet decode failed: source={}, bytes={}",
                                            source, size
                                        );
                                    }
                                }
                                Err(ref error)
                                    if error.kind() == std::io::ErrorKind::WouldBlock =>
                                {
                                    std::thread::sleep(Duration::from_millis(5));
                                }
                                Err(error) => {
                                    error!("OSC receive error: {error}");
                                }
                            }
                        }
                    })?,
            );
        }

        {
            let send_socket = UdpSocket::bind("0.0.0.0:0")?;
            let stop_send = stop.clone();
            threads.push(
                std::thread::Builder::new()
                    .name("tasq-osc-sender".to_string())
                    .spawn(move || {
                        while !stop_send.load(Ordering::Relaxed) {
                            match send_rx.recv_timeout(Duration::from_millis(50)) {
                                Ok(OscCommand::SendBatch {
                                    transaction_id,
                                    timing,
                                    queued_at,
                                    events,
                                }) => {
                                    let worker_started_at = Instant::now();
                                    let queue_wait_us = worker_started_at
                                        .saturating_duration_since(queued_at)
                                        .as_micros();
                                    let mut sent = 0usize;
                                    let mut failed = 0usize;
                                    let mut results = Vec::with_capacity(events.len());
                                    let mut first_send_started_at: Option<Instant> = None;
                                    let mut first_send_completed_at: Option<Instant> = None;
                                    let mut last_send_completed_at: Option<Instant> = None;
                                    let mut first_send_call_us: Option<u128> = None;
                                    let mut last_send_call_us: Option<u128> = None;
                                    let mut first_send_cue_id: Option<Uuid> = None;
                                    let mut last_send_cue_id: Option<Uuid> = None;

                                    for event in events {
                                        let batch_offset_us = worker_started_at
                                            .elapsed()
                                            .as_micros();
                                        let message = OscPacket::Message(OscMessage {
                                            addr: event.addr.clone(),
                                            args: event.args.clone(),
                                        });
                                        let encoded = match rosc::encoder::encode(&message) {
                                            Ok(bytes) => bytes,
                                            Err(error) => {
                                                failed += 1;
                                                results.push(OscSendResult {
                                                    cue_id: event.cue_id,
                                                    cue_number: event.cue_number,
                                                    cue_name: event.cue_name,
                                                    address: event.addr,
                                                    args: event.args,
                                                    status: format!("encode_failed: {error}"),
                                                    bytes_sent: 0,
                                                    batch_offset_us,
                                                    send_call_us: 0,
                                                });
                                                continue;
                                            }
                                        };

                                        // Timestamp the actual UDP API call. Logging is
                                        // deferred until the batch is complete so disk I/O
                                        // cannot delay the first OSC send.
                                        let send_started_at = Instant::now();
                                        if first_send_started_at.is_none() {
                                            first_send_started_at = Some(send_started_at);
                                            first_send_cue_id = Some(event.cue_id);
                                        }
                                        let send_result =
                                            send_socket.send_to(&encoded, &send_addr_clone);
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
                                        last_send_cue_id = Some(event.cue_id);

                                        match send_result {
                                            Ok(bytes_sent) => {
                                                sent += 1;
                                                results.push(OscSendResult {
                                                    cue_id: event.cue_id,
                                                    cue_number: event.cue_number,
                                                    cue_name: event.cue_name,
                                                    address: event.addr,
                                                    args: event.args,
                                                    status: "udp_send_accepted".to_string(),
                                                    bytes_sent,
                                                    batch_offset_us,
                                                    send_call_us,
                                                });
                                            }
                                            Err(error) => {
                                                failed += 1;
                                                results.push(OscSendResult {
                                                    cue_id: event.cue_id,
                                                    cue_number: event.cue_number,
                                                    cue_name: event.cue_name,
                                                    address: event.addr,
                                                    args: event.args,
                                                    status: format!("udp_send_failed: {error}"),
                                                    bytes_sent: 0,
                                                    batch_offset_us,
                                                    send_call_us,
                                                });
                                            }
                                        }
                                    }

                                    let worker_completed_at = Instant::now();
                                    let worker_elapsed_us = worker_completed_at
                                        .saturating_duration_since(worker_started_at)
                                        .as_micros();
                                    let first_to_last_send_span_us = match (
                                        first_send_started_at,
                                        last_send_completed_at,
                                    ) {
                                        (Some(first), Some(last)) => Some(
                                            last.saturating_duration_since(first).as_micros(),
                                        ),
                                        _ => None,
                                    };

                                    // Record each OSC Cue separately so the log shows
                                    // the exact address, arguments and UDP send result.
                                    // UDP send success is not a receiver acknowledgement.
                                    for result in &results {
                                        if result.status == "udp_send_accepted" {
                                            info!(
                                                "OSC Cue send result: transaction_id={}, cue_id={}, cue_number='{}', cue_name='{}', destination='{}', address='{}', args={:?}, status='{}', bytes_sent={}, batch_offset_us={}, send_call_us={}",
                                                transaction_id,
                                                result.cue_id,
                                                &result.cue_number,
                                                &result.cue_name,
                                                &send_addr_clone,
                                                &result.address,
                                                &result.args,
                                                &result.status,
                                                result.bytes_sent,
                                                result.batch_offset_us,
                                                result.send_call_us
                                            );
                                        } else {
                                            warn!(
                                                "OSC Cue send result: transaction_id={}, cue_id={}, cue_number='{}', cue_name='{}', destination='{}', address='{}', args={:?}, status='{}', bytes_sent={}, batch_offset_us={}, send_call_us={}",
                                                transaction_id,
                                                result.cue_id,
                                                &result.cue_number,
                                                &result.cue_name,
                                                &send_addr_clone,
                                                &result.address,
                                                &result.args,
                                                &result.status,
                                                result.bytes_sent,
                                                result.batch_offset_us,
                                                result.send_call_us
                                            );
                                        }
                                    }

                                    let go_to_worker_started_us =
                                        timing.go_elapsed_us(worker_started_at);
                                    let go_to_first_send_started_us = first_send_started_at
                                        .and_then(|at| timing.go_elapsed_us(at));
                                    let go_to_first_send_completed_us = first_send_completed_at
                                        .and_then(|at| timing.go_elapsed_us(at));
                                    let go_to_last_send_completed_us = last_send_completed_at
                                        .and_then(|at| timing.go_elapsed_us(at));
                                    let transaction_to_worker_started_us =
                                        timing.transaction_elapsed_us(worker_started_at);
                                    let transaction_to_first_send_started_us =
                                        first_send_started_at
                                            .map(|at| timing.transaction_elapsed_us(at));
                                    let transaction_to_first_send_completed_us =
                                        first_send_completed_at
                                            .map(|at| timing.transaction_elapsed_us(at));
                                    let transaction_to_last_send_completed_us =
                                        last_send_completed_at
                                            .map(|at| timing.transaction_elapsed_us(at));
                                    let worker_to_first_send_started_us = first_send_started_at
                                        .map(|at| at.saturating_duration_since(worker_started_at).as_micros());
                                    let worker_to_first_send_completed_us = first_send_completed_at
                                        .map(|at| at.saturating_duration_since(worker_started_at).as_micros());
                                    let worker_to_last_send_completed_us = last_send_completed_at
                                        .map(|at| at.saturating_duration_since(worker_started_at).as_micros());

                                    info!(
                                        target: "TaSQ::osc::server",
                                        "OSC dispatch latency: transaction_id={}, stage=LATENCY_SUMMARY, events={}, sent={}, failed={}, go_to_transaction_started_us={:?}, transaction_started_to_go_received_us={:?}, go_to_batch_queued_us={:?}, go_to_worker_started_us={:?}, go_to_first_send_started_us={:?}, go_to_first_send_completed_us={:?}, go_to_last_send_completed_us={:?}, transaction_to_batch_queued_us={}, batch_queued_to_worker_started_us={}, transaction_to_worker_started_us={}, transaction_to_first_send_started_us={:?}, transaction_to_first_send_completed_us={:?}, transaction_to_last_send_completed_us={:?}, queue_wait_us={}, worker_to_first_send_started_us={:?}, worker_to_first_send_completed_us={:?}, worker_to_last_send_completed_us={:?}, worker_elapsed_us={}, first_send_call_us={:?}, last_send_call_us={:?}, first_to_last_send_span_us={:?}, first_send_cue_id={:?}, last_send_cue_id={:?}, go_origin_present={}",
                                        transaction_id,
                                        results.len(),
                                        sent,
                                        failed,
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
                                            "OSC dispatch batch completed with errors: transaction_id={}, destination='{}', events={}, sent={}, failed={}, queue_wait_us={}, worker_elapsed_us={}",
                                            transaction_id,
                                            send_addr_clone,
                                            results.len(),
                                            sent,
                                            failed,
                                            queue_wait_us,
                                            worker_elapsed_us
                                        );
                                    } else {
                                        info!(
                                            "OSC dispatch batch completed: transaction_id={}, destination='{}', events={}, sent={}, failed={}, queue_wait_us={}, worker_elapsed_us={}",
                                            transaction_id,
                                            send_addr_clone,
                                            results.len(),
                                            sent,
                                            failed,
                                            queue_wait_us,
                                            worker_elapsed_us
                                        );
                                    }
                                }
                                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                            }
                        }
                    })?,
            );
        }

        Ok(Self {
            event_rx,
            command_tx,
            listen_port,
            send_addr,
            stop,
            threads,
        })
    }

    pub fn drain(&self) -> Vec<OscEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            out.push(event);
        }
        out
    }

    pub fn send_batch(
        &self,
        transaction_id: Uuid,
        timing: DispatchTiming,
        events: Vec<OscDispatch>,
    ) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let event_count = events.len();
        // Timestamp immediately before enqueue; the worker uses the same
        // monotonic clock to measure time spent waiting in its command queue.
        let queued_at = Instant::now();
        self.command_tx
            .try_send(OscCommand::SendBatch {
                transaction_id,
                timing,
                queued_at,
                events,
            })
            .map_err(|error| anyhow::anyhow!("Could not queue OSC dispatch batch: {error}"))?;
        info!(
            target: "TaSQ::osc::server",
            "OSC dispatch stage: transaction_id={} stage=BATCH_QUEUED events={} go_to_batch_queued_us={:?} transaction_to_batch_queued_us={} enqueue_call_us={}",
            transaction_id,
            event_count,
            timing.go_elapsed_us(queued_at),
            timing.transaction_elapsed_us(queued_at),
            queued_at.elapsed().as_micros()
        );
        Ok(())
    }

    #[allow(dead_code)]
    pub fn send(&self, addr: &str, args: Vec<OscType>) {
        let event = OscDispatch {
            cue_id: Uuid::nil(),
            cue_number: "external".to_string(),
            cue_name: "external".to_string(),
            addr: addr.to_string(),
            args,
        };
        let transaction_started_at = Instant::now();
        let timing = DispatchTiming {
            go_received_at: None,
            transaction_started_at,
        };
        if let Err(error) = self.send_batch(Uuid::new_v4(), timing, vec![event]) {
            warn!("Could not queue OSC message: {error}");
        }
    }
}

fn flatten_packet(packet: OscPacket, tx: &Sender<OscEvent>, source: SocketAddr) {
    match packet {
        OscPacket::Message(message) => {
            info!(
                "OSC message received: source={}, address='{}', args={:?}",
                source, message.addr, message.args
            );
            if tx
                .try_send(OscEvent {
                    addr: message.addr,
                    args: message.args,
                })
                .is_err()
            {
                warn!(
                    "OSC receive queue is full; message from {} was dropped",
                    source
                );
            }
        }
        OscPacket::Bundle(bundle) => {
            for packet in bundle.content {
                flatten_packet(packet, tx, source);
            }
        }
    }
}

impl Drop for OscServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}
