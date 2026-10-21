// Audio engine — manages CPAL device and mixing of multiple cue players

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Host, Stream, StreamConfig, SupportedStreamConfig};
use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;
use std::sync::Arc;
use tracing::{error, info, warn};

/// Commands sent from UI thread → audio thread
#[derive(Debug, Clone)]
pub enum AudioCommand {
    Play {
        id: uuid::Uuid,
        file: std::path::PathBuf,
        start_secs: f64,
        volume: f32,
        fade_in_secs: f32,
        loop_enabled: bool,
        loop_start: f64,
        loop_end: f64,
        loop_count: Option<u32>,
    },
    Stop(uuid::Uuid),
    StopAll,
    Pause(uuid::Uuid),
    PauseAll,
    Resume(uuid::Uuid),
    ResumeAll,
    SetVolume(uuid::Uuid, f32),
    FadeOut(uuid::Uuid, f32), // id, duration_secs
    Seek(uuid::Uuid, f64),
}

/// Position update sent from audio thread → UI thread
#[derive(Debug, Clone)]
pub struct PositionUpdate {
    pub id: uuid::Uuid,
    pub position_secs: f64,
    pub state: crate::cue::CueState,
}

pub struct AudioEngine {
    pub cmd_tx: Sender<AudioCommand>,
    pub pos_rx: Receiver<PositionUpdate>,
    pub devices: Vec<String>,
    pub current_device: String,
    _stream: Option<Arc<Mutex<Stream>>>,
}

impl AudioEngine {
    pub fn new() -> Result<Self> {
        let host = cpal::default_host();
        let devices = Self::list_output_devices(&host);
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("No output device found"))?;
        let device_name = device.name().unwrap_or_else(|_| "Default".to_string());

        info!("Audio device: {}", device_name);

        let (cmd_tx, cmd_rx) = bounded::<AudioCommand>(256);
        let (pos_tx, pos_rx) = bounded::<PositionUpdate>(1024);

        // Spawn audio worker thread
        std::thread::spawn(move || {
            audio_worker(device, cmd_rx, pos_tx);
        });

        Ok(Self {
            cmd_tx,
            pos_rx,
            devices,
            current_device: device_name,
            _stream: None,
        })
    }

    fn list_output_devices(host: &Host) -> Vec<String> {
        host.output_devices()
            .map(|devs| {
                devs.filter_map(|d| d.name().ok()).collect()
            })
            .unwrap_or_default()
    }

    pub fn send(&self, cmd: AudioCommand) {
        if let Err(e) = self.cmd_tx.send(cmd) {
            error!("Failed to send audio command: {}", e);
        }
    }

    pub fn drain_position_updates(&self) -> Vec<PositionUpdate> {
        let mut updates = Vec::new();
        while let Ok(u) = self.pos_rx.try_recv() {
            updates.push(u);
        }
        updates
    }
}

// ────────────────────────────────────────────────────────────
// Audio worker running on dedicated thread
// ────────────────────────────────────────────────────────────

struct ActivePlayer {
    id: uuid::Uuid,
    sink: rodio::Sink,
    duration_secs: f64,
    start_time: std::time::Instant,
    start_offset_secs: f64,
}

fn audio_worker(
    device: Device,
    cmd_rx: Receiver<AudioCommand>,
    pos_tx: Sender<PositionUpdate>,
) {
    let (_stream, stream_handle) = match rodio::OutputStream::try_from_device(&device) {
        Ok(v) => v,
        Err(e) => {
            error!("Could not open output stream: {e}");
            return;
        }
    };

    let mut players: Vec<ActivePlayer> = Vec::new();
    let tick = std::time::Duration::from_millis(50);

    loop {
        // Process all pending commands
        while let Ok(cmd) = cmd_rx.try_recv() {
            handle_command(cmd, &stream_handle, &mut players);
        }

        // Send position updates
        for p in &players {
            let elapsed = p.start_time.elapsed().as_secs_f64();
            let pos = (p.start_offset_secs + elapsed).min(p.duration_secs);
            let state = if p.sink.is_paused() {
                crate::cue::CueState::Paused
            } else if p.sink.empty() {
                crate::cue::CueState::Stopped
            } else {
                crate::cue::CueState::Playing
            };
            let _ = pos_tx.try_send(PositionUpdate {
                id: p.id,
                position_secs: pos,
                state,
            });
        }

        // Remove finished players
        players.retain(|p| !p.sink.empty() || p.sink.is_paused());

        std::thread::sleep(tick);
    }
}

fn handle_command(
    cmd: AudioCommand,
    stream_handle: &rodio::OutputStreamHandle,
    players: &mut Vec<ActivePlayer>,
) {
    match cmd {
        AudioCommand::Play {
            id,
            file,
            start_secs,
            volume,
            fade_in_secs: _,
            ..
        } => {
            // Stop existing player for this id if any
            players.retain(|p| {
                if p.id == id {
                    p.sink.stop();
                    false
                } else {
                    true
                }
            });

            let file = match std::fs::File::open(&file) {
                Ok(f) => f,
                Err(e) => {
                    error!("Cannot open {:?}: {e}", file);
                    return;
                }
            };

            let sink = match rodio::Sink::try_new(stream_handle) {
                Ok(s) => s,
                Err(e) => {
                    error!("Cannot create sink: {e}");
                    return;
                }
            };

            let source = match rodio::Decoder::new(std::io::BufReader::new(file)) {
                Ok(s) => s,
                Err(e) => {
                    error!("Cannot decode audio: {e}");
                    return;
                }
            };

            sink.set_volume(volume);
            sink.append(source);
            sink.play();

            players.push(ActivePlayer {
                id,
                sink,
                duration_secs: 0.0, // will be set from metadata
                start_time: std::time::Instant::now(),
                start_offset_secs: start_secs,
            });
        }

        AudioCommand::Stop(id) => {
            if let Some(p) = players.iter().find(|p| p.id == id) {
                p.sink.stop();
            }
            players.retain(|p| p.id != id);
        }

        AudioCommand::StopAll => {
            for p in players.iter() {
                p.sink.stop();
            }
            players.clear();
        }

        AudioCommand::Pause(id) => {
            if let Some(p) = players.iter().find(|p| p.id == id) {
                p.sink.pause();
            }
        }

        AudioCommand::PauseAll => {
            for p in players.iter() {
                p.sink.pause();
            }
        }

        AudioCommand::Resume(id) => {
            if let Some(p) = players.iter().find(|p| p.id == id) {
                p.sink.play();
            }
        }

        AudioCommand::ResumeAll => {
            for p in players.iter() {
                p.sink.play();
            }
        }

        AudioCommand::SetVolume(id, vol) => {
            if let Some(p) = players.iter().find(|p| p.id == id) {
                p.sink.set_volume(vol);
            }
        }

        AudioCommand::FadeOut(id, _dur) => {
            // Simple stop for now; full fade implemented in Phase 2
            if let Some(p) = players.iter().find(|p| p.id == id) {
                p.sink.stop();
            }
            players.retain(|p| p.id != id);
        }

        AudioCommand::Seek(_id, _pos) => {
            // TODO: Phase 2 — seek support
        }
    }
}
