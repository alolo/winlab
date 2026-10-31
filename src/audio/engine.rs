// src/audio/engine.rs — аудиодвижок с поддержкой Fade In/Out, Start/End, Loop и смены I/O карт

use crate::cue::{CueState, FadeCurve};
use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, Host};
use crossbeam_channel::{bounded, Receiver, Sender};
use rodio::Source;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum AudioCommand {
    Play {
        id: Uuid,
        file: PathBuf,
        start_secs: f64,
        end_secs: Option<f64>,
        duration_secs: f64,
        volume: f32,
        fade_in_secs: f64,
        fade_out_secs: f64,
        fade_curve: FadeCurve,
        loop_enabled: bool,
        loop_start: f64,
        loop_end: f64,
        loop_count: i32, // -1 = бесконечный, >0 = число повторов
    },
    Stop(Uuid),
    StopAll,
    Pause(Uuid),
    PauseAll,
    Resume(Uuid),
    ResumeAll,
    SetVolume(Uuid, f32),
    Fade {
        id: Uuid,
        target_volume: f32,
        duration_secs: f64,
        stop_on_finish: bool,
    },
    SetOutputDevice(String),
    SetInputDevice(String),
}

#[derive(Debug, Clone)]
pub struct PositionUpdate {
    pub id: Uuid,
    pub position_secs: f64,
    pub state: CueState,
}

pub struct AudioEngine {
    pub cmd_tx: Sender<AudioCommand>,
    pub pos_rx: Receiver<PositionUpdate>,
    pub output_devices: Vec<String>,
    pub input_devices: Vec<String>,
    pub current_device: String,
    pub current_input_device: String,
}

impl AudioEngine {
    pub fn new() -> Result<Self> {
        let host = cpal::default_host();
        let output_devices = Self::list_output_devices(&host);
        let input_devices = Self::list_input_devices(&host);

        let out_dev = host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("No output audio device found"))?;
        let out_name = out_dev.name().unwrap_or_else(|_| "Default Output".to_string());

        let in_name = host
            .default_input_device()
            .and_then(|d| d.name().ok())
            .unwrap_or_else(|| "Default Input".to_string());

        info!("Audio Output: {}, Input: {}", out_name, in_name);

        let (cmd_tx, cmd_rx) = bounded::<AudioCommand>(256);
        let (pos_tx, pos_rx) = bounded::<PositionUpdate>(1024);

        std::thread::spawn(move || {
            audio_worker(out_dev, cmd_rx, pos_tx);
        });

        Ok(Self {
            cmd_tx,
            pos_rx,
            output_devices,
            input_devices,
            current_device: out_name,
            current_input_device: in_name,
        })
    }

    fn list_output_devices(host: &Host) -> Vec<String> {
        host.output_devices()
            .map(|devs| devs.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default()
    }

    fn list_input_devices(host: &Host) -> Vec<String> {
        host.input_devices()
            .map(|devs| devs.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default()
    }

    pub fn set_output_device(&mut self, name: &str) {
        self.current_device = name.to_string();
        self.send(AudioCommand::SetOutputDevice(name.to_string()));
    }

    pub fn set_input_device(&mut self, name: &str) {
        self.current_input_device = name.to_string();
        self.send(AudioCommand::SetInputDevice(name.to_string()));
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

struct DynamicFade {
    start_vol: f32,
    target_vol: f32,
    start_time: Instant,
    duration_secs: f64,
    stop_on_finish: bool,
}

struct ActivePlayer {
    id: Uuid,
    file: PathBuf,
    sink: rodio::Sink,
    start_secs: f64,
    end_secs: f64,
    base_volume: f32,
    current_volume: f32,
    fade_in_secs: f64,
    fade_out_secs: f64,
    fade_curve: FadeCurve,
    loop_enabled: bool,
    loop_start: f64,
    loop_end: f64,
    loops_remaining: i32, // -1 = inf
    segment_start_pos: f64,
    segment_instant: Instant,
    paused_at: Option<f64>,
    dynamic_fade: Option<DynamicFade>,
}

impl ActivePlayer {
    fn current_pos(&self) -> f64 {
        if let Some(p) = self.paused_at {
            p
        } else {
            self.segment_start_pos + self.segment_instant.elapsed().as_secs_f64()
        }
    }
}

fn open_sink_at(
    stream_handle: &rodio::OutputStreamHandle,
    file_path: &PathBuf,
    offset_secs: f64,
    initial_vol: f32,
) -> Option<rodio::Sink> {
    let file = std::fs::File::open(file_path).ok()?;
    let sink = rodio::Sink::try_new(stream_handle).ok()?;
    let decoder = rodio::Decoder::new(std::io::BufReader::new(file)).ok()?;
    sink.set_volume(initial_vol);
    if offset_secs > 0.0 {
        sink.append(decoder.skip_duration(Duration::from_secs_f64(offset_secs)));
    } else {
        sink.append(decoder);
    }
    sink.play();
    Some(sink)
}

fn audio_worker(
    initial_device: Device,
    cmd_rx: Receiver<AudioCommand>,
    pos_tx: Sender<PositionUpdate>,
) {
    let mut current_stream = rodio::OutputStream::try_from_device(&initial_device).ok();
    if current_stream.is_none() {
        error!("Could not open initial audio output stream");
        return;
    }

    let mut players: Vec<ActivePlayer> = Vec::new();
    let tick = Duration::from_millis(20);

    loop {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                AudioCommand::SetOutputDevice(dev_name) => {
                    let host = cpal::default_host();
                    if let Ok(mut devs) = host.output_devices() {
                        if let Some(dev) = devs.find(|d| d.name().ok().as_deref() == Some(&dev_name))
                        {
                            for p in &players {
                                p.sink.stop();
                            }
                            players.clear();
                            if let Ok(new_stream) = rodio::OutputStream::try_from_device(&dev) {
                                current_stream = Some(new_stream);
                                info!("Switched output audio device to: {}", dev_name);
                            }
                        }
                    }
                }
                AudioCommand::SetInputDevice(dev_name) => {
                    info!("Selected input audio device: {}", dev_name);
                }
                other => {
                    if let Some((_, ref handle)) = current_stream {
                        handle_command(other, handle, &mut players);
                    }
                }
            }
        }

        let mut finished_ids = Vec::new();

        if let Some((_, ref handle)) = current_stream {
            for p in &mut players {
                if p.sink.is_paused() {
                    let _ = pos_tx.try_send(PositionUpdate {
                        id: p.id,
                        position_secs: p.current_pos(),
                        state: CueState::Paused,
                    });
                    continue;
                }

                let mut pos = p.current_pos();

                // Проверка цикла (Loop)
                let valid_loop = p.loop_enabled && p.loop_end > p.loop_start;
                if valid_loop && pos >= p.loop_end {
                    if p.loops_remaining == -1 || p.loops_remaining > 1 {
                        if p.loops_remaining > 1 {
                            p.loops_remaining -= 1;
                        }
                        p.sink.stop();
                        if let Some(new_sink) =
                            open_sink_at(handle, &p.file, p.loop_start, p.current_volume)
                        {
                            p.sink = new_sink;
                            p.segment_start_pos = p.loop_start;
                            p.segment_instant = Instant::now();
                            pos = p.loop_start;
                        } else {
                            finished_ids.push(p.id);
                            continue;
                        }
                    }
                }

                // Проверка точки окончания (Custom End Point или конец файла)
                if (p.end_secs > 0.0 && pos >= p.end_secs) || p.sink.empty() {
                    p.sink.stop();
                    finished_ids.push(p.id);
                    continue;
                }

                // Расчет огибающей громкости (Fade In / Fade Out / Dynamic Fade)
                let mut state = CueState::Playing;
                let mut env = 1.0_f32;

                let elapsed_from_start = (pos - p.start_secs).max(0.0);
                if p.fade_in_secs > 0.0 && elapsed_from_start < p.fade_in_secs {
                    let t = (elapsed_from_start / p.fade_in_secs) as f32;
                    env *= p.fade_curve.apply(t);
                    state = CueState::FadingIn;
                }

                if p.end_secs > 0.0 && p.fade_out_secs > 0.0 {
                    let remaining = (p.end_secs - pos).max(0.0);
                    if remaining < p.fade_out_secs {
                        let t = (remaining / p.fade_out_secs) as f32;
                        env *= p.fade_curve.apply(t);
                        state = CueState::FadingOut;
                    }
                }

                if let Some(ref df) = p.dynamic_fade {
                    let df_elapsed = df.start_time.elapsed().as_secs_f64();
                    if df_elapsed >= df.duration_secs {
                        p.base_volume = df.target_vol;
                        let should_stop = df.stop_on_finish;
                        p.dynamic_fade = None;
                        if should_stop {
                            p.sink.stop();
                            finished_ids.push(p.id);
                            continue;
                        }
                    } else {
                        let t = (df_elapsed / df.duration_secs.max(0.001)) as f32;
                        let interp = df.start_vol + (df.target_vol - df.start_vol) * t;
                        p.current_volume = interp * env;
                        p.sink.set_volume(p.current_volume);
                        state = if df.target_vol >= df.start_vol {
                            CueState::FadingIn
                        } else {
                            CueState::FadingOut
                        };
                        let _ = pos_tx.try_send(PositionUpdate {
                            id: p.id,
                            position_secs: pos,
                            state,
                        });
                        continue;
                    }
                }

                p.current_volume = p.base_volume * env;
                p.sink.set_volume(p.current_volume);

                let _ = pos_tx.try_send(PositionUpdate {
                    id: p.id,
                    position_secs: pos,
                    state,
                });
            }
        }

        for fin_id in finished_ids {
            players.retain(|p| p.id != fin_id);
            let _ = pos_tx.try_send(PositionUpdate {
                id: fin_id,
                position_secs: 0.0,
                state: CueState::Stopped,
            });
        }

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
            end_secs,
            duration_secs,
            volume,
            fade_in_secs,
            fade_out_secs,
            fade_curve,
            loop_enabled,
            loop_start,
            loop_end,
            loop_count,
        } => {
            players.retain(|p| {
                if p.id == id {
                    p.sink.stop();
                    false
                } else {
                    true
                }
            });

            let initial_vol = if fade_in_secs > 0.0 { 0.0 } else { volume };
            let Some(sink) = open_sink_at(stream_handle, &file, start_secs, initial_vol) else {
                warn!("Cannot open audio file for playback: {:?}", file);
                return;
            };

            let eff_end = end_secs
                .unwrap_or(duration_secs)
                .min(if duration_secs > 0.0 { duration_secs } else { f64::MAX });

            let l_end = if loop_end > loop_start {
                loop_end
            } else {
                eff_end
            };

            players.push(ActivePlayer {
                id,
                file,
                sink,
                start_secs,
                end_secs: if eff_end == f64::MAX { 0.0 } else { eff_end },
                base_volume: volume,
                current_volume: initial_vol,
                fade_in_secs,
                fade_out_secs,
                fade_curve,
                loop_enabled,
                loop_start,
                loop_end: l_end,
                loops_remaining: loop_count,
                segment_start_pos: start_secs,
                segment_instant: Instant::now(),
                paused_at: None,
                dynamic_fade: None,
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
            if let Some(p) = players.iter_mut().find(|p| p.id == id) {
                let pos = p.current_pos();
                p.sink.pause();
                p.paused_at = Some(pos);
            }
        }

        AudioCommand::PauseAll => {
            for p in players.iter_mut() {
                let pos = p.current_pos();
                p.sink.pause();
                p.paused_at = Some(pos);
            }
        }

        AudioCommand::Resume(id) => {
            if let Some(p) = players.iter_mut().find(|p| p.id == id) {
                if let Some(pos) = p.paused_at.take() {
                    p.segment_start_pos = pos;
                    p.segment_instant = Instant::now();
                }
                p.sink.play();
            }
        }

        AudioCommand::ResumeAll => {
            for p in players.iter_mut() {
                if let Some(pos) = p.paused_at.take() {
                    p.segment_start_pos = pos;
                    p.segment_instant = Instant::now();
                }
                p.sink.play();
            }
        }

        AudioCommand::SetVolume(id, vol) => {
            if let Some(p) = players.iter_mut().find(|p| p.id == id) {
                p.base_volume = vol;
                p.sink.set_volume(vol);
            }
        }

        AudioCommand::Fade {
            id,
            target_volume,
            duration_secs,
            stop_on_finish,
        } => {
            if let Some(p) = players.iter_mut().find(|p| p.id == id) {
                p.dynamic_fade = Some(DynamicFade {
                    start_vol: p.current_volume,
                    target_vol: target_volume,
                    start_time: Instant::now(),
                    duration_secs,
                    stop_on_finish,
                });
            }
        }

        AudioCommand::SetOutputDevice(_) | AudioCommand::SetInputDevice(_) => {}
    }
}