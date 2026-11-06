// src/audio/engine.rs — аудиодвижок с поддержкой Fade In/Out, Start/End, Loop и смены I/O карт

use crate::cue::{ChannelRoute, CueState, FadeCurve, OutputChannelSetting};
use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, Host};
use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::RwLock;
use rodio::Source;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum AudioCommand {
    Play {
        id: Uuid,
        file: PathBuf,
        /// None uses the global output device; Some selects a per-cue device.
        output_device: Option<String>,
        start_secs: f64,
        end_secs: Option<f64>,
        duration_secs: f64,
        volume: f32,
        fade_in_secs: f64,
        fade_out_secs: f64,
        fade_curve: FadeCurve,
        custom_fade_points: Vec<[f32; 2]>,
        channel_routes: Vec<ChannelRoute>,
        loop_enabled: bool,
        loop_start: f64,
        loop_end: f64,
        loop_count: i32, // -1 = бесконечный, >0 = число повторов
    },
    Stop(Uuid),
    StopAll,
    Pause(Uuid),
    Seek {
        id: Uuid,
        position_secs: f64,
    },
    PauseAll,
    Resume(Uuid),
    ResumeAll,
    SetVolume(Uuid, f32),
    Fade {
        id: Uuid,
        target_volume: f32,
        duration_secs: f64,
        stop_on_finish: bool,
        curve: FadeCurve,
        custom_fade_points: Vec<[f32; 2]>,
    },
    SetOutputDevice(String),
    SetInputDevice(String),
    SetRouting {
        id: Uuid,
        routes: Vec<ChannelRoute>,
    },
}

#[derive(Debug, Clone)]
pub struct PositionUpdate {
    pub id: Uuid,
    pub position_secs: f64,
    pub state: CueState,
}

fn supported_output_channel_layouts(device: &Device) -> Vec<usize> {
    let mut layouts: Vec<usize> = device
        .supported_output_configs()
        .map(|configs| configs.map(|config| config.channels() as usize).collect())
        .unwrap_or_default();
    if let Ok(default_config) = device.default_output_config() {
        layouts.push(default_config.channels() as usize);
    }
    layouts.retain(|channels| *channels > 0);
    layouts.sort_unstable();
    layouts.dedup();
    layouts
}

fn preferred_output_config(device: &Device) -> Option<cpal::SupportedStreamConfig> {
    // Select the configuration with the greatest number of channels first;
    // prefer F32 only when two configurations expose the same channel count.
    let config = device
        .supported_output_configs()
        .ok()?
        .max_by_key(|config| {
            (
                config.channels(),
                (config.sample_format() == cpal::SampleFormat::F32) as u8,
            )
        })?;
    let min_rate = config.min_sample_rate().0;
    let max_rate = config.max_sample_rate().0;
    let sample_rate = if min_rate <= 48_000 && max_rate >= 48_000 {
        48_000
    } else if min_rate <= 44_100 && max_rate >= 44_100 {
        44_100
    } else {
        max_rate
    };
    Some(config.with_sample_rate(cpal::SampleRate(sample_rate)))
}

pub struct AudioEngine {
    pub cmd_tx: Sender<AudioCommand>,
    pub pos_rx: Receiver<PositionUpdate>,
    pub output_devices: Vec<String>,
    pub input_devices: Vec<String>,
    pub current_device: String,
    pub current_input_device: String,
    pub output_channels: usize,
    output_device_channels: HashMap<String, usize>,
    output_device_channel_layouts: HashMap<String, Vec<usize>>,
    output_channel_settings: Arc<RwLock<Vec<OutputChannelSetting>>>,
}

impl AudioEngine {
    pub fn new() -> Result<Self> {
        let host = cpal::default_host();
        let output_devices = Self::list_output_devices(&host);
        let (output_device_channels, output_device_channel_layouts) =
            Self::list_output_device_info(&host);
        let input_devices = Self::list_input_devices(&host);

        let out_dev = host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("No output audio device found"))?;
        let out_name = out_dev
            .name()
            .unwrap_or_else(|_| "Default Output".to_string());
        let output_config = preferred_output_config(&out_dev)
            .or_else(|| out_dev.default_output_config().ok())
            .ok_or_else(|| anyhow::anyhow!("No supported output configuration found"))?;
        let output_channels = output_config.channels() as usize;

        let in_name = host
            .default_input_device()
            .and_then(|d| d.name().ok())
            .unwrap_or_else(|| "Default Input".to_string());

        info!("Audio Output: {}, Input: {}", out_name, in_name);

        let (cmd_tx, cmd_rx) = bounded::<AudioCommand>(256);
        let (pos_tx, pos_rx) = bounded::<PositionUpdate>(1024);
        let output_channel_settings = Arc::new(RwLock::new(vec![
            OutputChannelSetting::default();
            output_channels.max(1)
        ]));
        let worker_output_settings = output_channel_settings.clone();
        let worker_default_device = out_name.clone();

        std::thread::spawn(move || {
            audio_worker(
                out_dev,
                output_config,
                worker_default_device,
                cmd_rx,
                pos_tx,
                worker_output_settings,
            );
        });

        Ok(Self {
            cmd_tx,
            pos_rx,
            output_devices,
            input_devices,
            current_device: out_name,
            current_input_device: in_name,
            output_channels,
            output_device_channels,
            output_device_channel_layouts,
            output_channel_settings,
        })
    }

    fn list_output_devices(host: &Host) -> Vec<String> {
        host.output_devices()
            .map(|devs| devs.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default()
    }

    fn list_output_device_info(
        host: &Host,
    ) -> (HashMap<String, usize>, HashMap<String, Vec<usize>>) {
        let mut max_channels = HashMap::new();
        let mut layouts_by_device = HashMap::new();
        if let Ok(devices) = host.output_devices() {
            for device in devices {
                let Ok(name) = device.name() else {
                    continue;
                };
                let layouts = supported_output_channel_layouts(&device);
                let channels = layouts
                    .last()
                    .copied()
                    .or_else(|| {
                        device
                            .default_output_config()
                            .ok()
                            .map(|config| config.channels() as usize)
                    })
                    .unwrap_or(2)
                    .max(1);
                info!(
                    "Output device '{}' reports channel layouts: {:?}; selected maximum: {}",
                    name, layouts, channels
                );
                max_channels.insert(name.clone(), channels);
                layouts_by_device.insert(name, layouts);
            }
        }
        (max_channels, layouts_by_device)
    }

    /// Refresh the channel layouts for a device when it is selected. This avoids
    /// keeping stale channel counts if the Windows endpoint configuration changed
    /// after CueLabs started.
    pub fn refresh_output_device_info(&mut self, device_name: &str) {
        let host = cpal::default_host();
        let Ok(devices) = host.output_devices() else {
            return;
        };
        let Some(device) = devices
            .into_iter()
            .find(|device| device.name().ok().as_deref() == Some(device_name))
        else {
            return;
        };
        let layouts = supported_output_channel_layouts(&device);
        let channels = layouts
            .last()
            .copied()
            .or_else(|| {
                device
                    .default_output_config()
                    .ok()
                    .map(|config| config.channels() as usize)
            })
            .unwrap_or(2)
            .max(1);
        info!(
            "Refreshed output device '{}' channel layouts: {:?}; max={}",
            device_name, layouts, channels
        );
        self.output_device_channels
            .insert(device_name.to_string(), channels);
        self.output_device_channel_layouts
            .insert(device_name.to_string(), layouts);
        if self.current_device == device_name {
            self.output_channels = channels;
            let mut settings = self.output_channel_settings.write();
            settings.resize(channels, OutputChannelSetting::default());
            settings.truncate(channels);
        }
    }

    pub fn output_channel_layouts_for_device(&self, device_name: Option<&str>) -> Vec<usize> {
        let name = device_name.unwrap_or(self.current_device.as_str());
        self.output_device_channel_layouts
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    /// Physical output count for a selected cue device. `None` means global default.
    pub fn output_channels_for_device(&self, device_name: Option<&str>) -> usize {
        match device_name {
            Some(name) => self
                .output_device_channels
                .get(name)
                .copied()
                .unwrap_or(self.output_channels)
                .max(1),
            None => self.output_channels.max(1),
        }
    }

    fn list_input_devices(host: &Host) -> Vec<String> {
        host.input_devices()
            .map(|devs| devs.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default()
    }

    pub fn set_output_device(&mut self, name: &str) {
        self.refresh_output_device_info(name);
        self.output_channels = self
            .output_device_channels
            .get(name)
            .copied()
            .unwrap_or(2)
            .max(1);
        {
            let mut settings = self.output_channel_settings.write();
            settings.resize(self.output_channels, OutputChannelSetting::default());
            settings.truncate(self.output_channels);
        }
        self.current_device = name.to_string();
        self.send(AudioCommand::SetOutputDevice(name.to_string()));
    }

    pub fn output_channel_settings(&self) -> Vec<OutputChannelSetting> {
        self.output_channel_settings.read().clone()
    }

    pub fn set_output_channel_settings(&self, settings: Vec<OutputChannelSetting>) {
        let mut current = self.output_channel_settings.write();
        *current = settings;
        current.resize(self.output_channels.max(1), OutputChannelSetting::default());
        current.truncate(self.output_channels.max(1));
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
    curve: FadeCurve,
    custom_fade_points: Vec<[f32; 2]>,
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
    custom_fade_points: Vec<[f32; 2]>,
    channel_routes: Arc<RwLock<Vec<ChannelRoute>>>,
    output_channels: usize,
    output_device: String,
    uses_global_default: bool,
    stream_handle: Arc<rodio::OutputStreamHandle>,
    output_settings: Arc<RwLock<Vec<OutputChannelSetting>>>,
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

fn db_to_linear(db: f32) -> f32 {
    if db <= -60.0 {
        0.0
    } else {
        10.0_f32.powf(db / 20.0)
    }
}

struct ChannelRouter<S> {
    source: S,
    input_channels: usize,
    output_channels: usize,
    output_index: usize,
    input_frame: Vec<f32>,
    output_frame: Vec<f32>,
    routes: Arc<RwLock<Vec<ChannelRoute>>>,
    output_settings: Arc<RwLock<Vec<OutputChannelSetting>>>,
}

impl<S> ChannelRouter<S>
where
    S: Source<Item = f32>,
{
    fn new(
        source: S,
        routes: Arc<RwLock<Vec<ChannelRoute>>>,
        output_settings: Arc<RwLock<Vec<OutputChannelSetting>>>,
        output_channels: usize,
    ) -> Self {
        let input_channels = source.channels().max(1) as usize;
        let output_channels = output_channels.max(1);

        // Keep the route table aligned with the decoder's real channel count.
        // This also provides a deterministic default map for legacy projects
        // whose per-track route list was empty or shorter than the audio stream.
        {
            let mut route_list = routes.write();
            route_list.truncate(input_channels);
            while route_list.len() < input_channels {
                let source_index = route_list.len();
                route_list.push(ChannelRoute {
                    active: true,
                    output_channel: source_index.min(output_channels - 1),
                    gain_db: 0.0,
                });
            }
            for route in route_list.iter_mut() {
                route.output_channel = route.output_channel.min(output_channels - 1);
            }
            info!(
                "ChannelRouter configured: {} source channels -> {} hardware outputs; routes={:?}",
                input_channels, output_channels, &*route_list
            );
        }

        Self {
            source,
            input_channels,
            output_channels,
            output_index: output_channels,
            input_frame: vec![0.0; input_channels],
            output_frame: vec![0.0; output_channels],
            routes,
            output_settings,
        }
    }
}

impl<S> Iterator for ChannelRouter<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.output_index >= self.output_channels {
            for sample in &mut self.input_frame {
                *sample = self.source.next()?;
            }
            self.output_frame.fill(0.0);

            let routes = self.routes.read();
            let outputs = self.output_settings.read();
            for (source_index, sample) in self.input_frame.iter().copied().enumerate() {
                let fallback_route = ChannelRoute {
                    active: true,
                    output_channel: source_index.min(self.output_channels - 1),
                    gain_db: 0.0,
                };
                let route = routes.get(source_index).unwrap_or(&fallback_route);
                if !route.active || route.output_channel >= self.output_channels {
                    continue;
                }

                let output_setting = outputs.get(route.output_channel);
                if output_setting
                    .map(|setting| !setting.active)
                    .unwrap_or(false)
                {
                    continue;
                }
                let output_gain = output_setting
                    .map(|setting| db_to_linear(setting.gain_db))
                    .unwrap_or(1.0);
                self.output_frame[route.output_channel] +=
                    sample * db_to_linear(route.gain_db) * output_gain;
            }
            self.output_index = 0;
        }

        let sample = self.output_frame[self.output_index];
        self.output_index += 1;
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let (lower, upper) = self.source.size_hint();
        let pending = self.output_channels.saturating_sub(self.output_index);
        let lower_frames = lower / self.input_channels;
        let lower = lower_frames
            .saturating_mul(self.output_channels)
            .saturating_add(pending);
        let upper = upper.map(|samples| {
            (samples / self.input_channels)
                .saturating_mul(self.output_channels)
                .saturating_add(pending)
        });
        (lower, upper)
    }
}

impl<S> Source for ChannelRouter<S>
where
    S: Source<Item = f32>,
{
    fn current_frame_len(&self) -> Option<usize> {
        self.source.current_frame_len().map(|samples| {
            (samples / self.input_channels)
                .saturating_mul(self.output_channels)
                .saturating_add(self.output_channels.saturating_sub(self.output_index))
        })
    }

    fn channels(&self) -> u16 {
        self.output_channels.min(u16::MAX as usize) as u16
    }

    fn sample_rate(&self) -> u32 {
        self.source.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.source.total_duration()
    }
}

fn open_sink_at(
    stream_handle: &rodio::OutputStreamHandle,
    file_path: &PathBuf,
    offset_secs: f64,
    initial_vol: f32,
    output_channels: usize,
    routes: Arc<RwLock<Vec<ChannelRoute>>>,
    output_settings: Arc<RwLock<Vec<OutputChannelSetting>>>,
) -> Option<rodio::Sink> {
    let file = std::fs::File::open(file_path).ok()?;
    let sink = rodio::Sink::try_new(stream_handle).ok()?;
    let decoder = rodio::Decoder::new(std::io::BufReader::new(file)).ok()?;
    let source = decoder
        .skip_duration(Duration::from_secs_f64(offset_secs.max(0.0)))
        .convert_samples::<f32>();
    let routed = ChannelRouter::new(source, routes, output_settings, output_channels);
    sink.set_volume(initial_vol);
    sink.append(routed);
    sink.play();
    Some(sink)
}

struct DeviceOutput {
    // Keeping the stream alive is required for the device's output callback.
    _stream: rodio::OutputStream,
    handle: Arc<rodio::OutputStreamHandle>,
    channels: usize,
    settings: Arc<RwLock<Vec<OutputChannelSetting>>>,
}

fn open_device_output(
    device_name: &str,
    host: &Host,
    default_device_name: &str,
    default_settings: &Arc<RwLock<Vec<OutputChannelSetting>>>,
) -> Option<DeviceOutput> {
    let devices = host.output_devices().ok()?;
    let device = devices
        .into_iter()
        .find(|device| device.name().ok().as_deref() == Some(device_name))?;
    let config =
        preferred_output_config(&device).or_else(|| device.default_output_config().ok())?;
    let channels = (config.channels() as usize).max(1);
    let (stream, handle) = rodio::OutputStream::try_from_device_config(&device, config).ok()?;
    let settings = if device_name == default_device_name {
        default_settings.clone()
    } else {
        Arc::new(RwLock::new(vec![OutputChannelSetting::default(); channels]))
    };
    Some(DeviceOutput {
        _stream: stream,
        handle: Arc::new(handle),
        channels,
        settings,
    })
}

fn ensure_device_output(
    streams: &mut HashMap<String, DeviceOutput>,
    device_name: &str,
    host: &Host,
    default_device_name: &str,
    default_settings: &Arc<RwLock<Vec<OutputChannelSetting>>>,
) -> Option<(
    Arc<rodio::OutputStreamHandle>,
    usize,
    Arc<RwLock<Vec<OutputChannelSetting>>>,
)> {
    if !streams.contains_key(device_name) {
        let output = open_device_output(device_name, host, default_device_name, default_settings)?;
        streams.insert(device_name.to_string(), output);
    }
    let output = streams.get_mut(device_name)?;
    if device_name == default_device_name {
        output.settings = default_settings.clone();
    }
    Some((
        output.handle.clone(),
        output.channels,
        output.settings.clone(),
    ))
}

fn audio_worker(
    initial_device: Device,
    initial_config: cpal::SupportedStreamConfig,
    initial_device_name: String,
    cmd_rx: Receiver<AudioCommand>,
    pos_tx: Sender<PositionUpdate>,
    output_settings: Arc<RwLock<Vec<OutputChannelSetting>>>,
) {
    let initial_channels = (initial_config.channels() as usize).max(1);
    let (initial_stream, initial_handle) =
        match rodio::OutputStream::try_from_device_config(&initial_device, initial_config) {
            Ok(stream) => stream,
            Err(error) => {
                error!("Could not open initial audio output stream: {}", error);
                return;
            }
        };

    let mut streams = HashMap::new();
    streams.insert(
        initial_device_name.clone(),
        DeviceOutput {
            _stream: initial_stream,
            handle: Arc::new(initial_handle),
            channels: initial_channels,
            settings: output_settings.clone(),
        },
    );
    let mut default_device_name = initial_device_name;
    let host = cpal::default_host();
    let mut players: Vec<ActivePlayer> = Vec::new();
    let tick = Duration::from_millis(5);

    loop {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                AudioCommand::SetOutputDevice(dev_name) => {
                    // Stop cues that follow the global default, but leave cues pinned
                    // to an explicit per-cue device running on their own streams.
                    players.retain(|player| {
                        if player.uses_global_default {
                            info!(
                                "Stopping cue {} on previous global output '{}'",
                                player.id, player.output_device
                            );
                            player.sink.stop();
                            false
                        } else {
                            true
                        }
                    });
                    default_device_name = dev_name.clone();
                    if let Some((_, channels, settings)) = ensure_device_output(
                        &mut streams,
                        &dev_name,
                        &host,
                        &default_device_name,
                        &output_settings,
                    ) {
                        let mut current = settings.write();
                        current.resize(channels, OutputChannelSetting::default());
                        current.truncate(channels);
                        info!(
                            "Switched global output device to: {} ({} channels)",
                            dev_name, channels
                        );
                    } else {
                        warn!(
                            "Could not open selected output configuration for device: {}",
                            dev_name
                        );
                    }
                }
                AudioCommand::SetInputDevice(dev_name) => {
                    info!("Selected input audio device: {}", dev_name);
                }
                other => handle_command(
                    other,
                    &mut players,
                    &mut streams,
                    &host,
                    &default_device_name,
                    &output_settings,
                ),
            }
        }

        let mut finished_ids = Vec::new();
        for player in &mut players {
            if player.sink.is_paused() {
                let _ = pos_tx.try_send(PositionUpdate {
                    id: player.id,
                    position_secs: player.current_pos(),
                    state: CueState::Paused,
                });
                continue;
            }

            let mut pos = player.current_pos();

            // Loop restart must use the same output device as the original cue.
            let valid_loop = player.loop_enabled && player.loop_end > player.loop_start;
            if valid_loop && pos >= player.loop_end {
                if player.loops_remaining == -1 || player.loops_remaining > 1 {
                    if player.loops_remaining > 1 {
                        player.loops_remaining -= 1;
                    }
                    player.sink.stop();
                    if let Some(new_sink) = open_sink_at(
                        player.stream_handle.as_ref(),
                        &player.file,
                        player.loop_start,
                        player.current_volume,
                        player.output_channels,
                        player.channel_routes.clone(),
                        player.output_settings.clone(),
                    ) {
                        player.sink = new_sink;
                        player.segment_start_pos = player.loop_start;
                        player.segment_instant = Instant::now();
                        pos = player.loop_start;
                    } else {
                        finished_ids.push(player.id);
                        continue;
                    }
                }
            }

            if (player.end_secs > 0.0 && pos >= player.end_secs) || player.sink.empty() {
                player.sink.stop();
                finished_ids.push(player.id);
                continue;
            }

            let mut state = CueState::Playing;
            let mut env = 1.0_f32;
            let elapsed_from_start = (pos - player.start_secs).max(0.0);
            if player.fade_in_secs > 0.0 && elapsed_from_start < player.fade_in_secs {
                let t = (elapsed_from_start / player.fade_in_secs) as f32;
                env *= player
                    .fade_curve
                    .apply_with_points(t, &player.custom_fade_points);
                state = CueState::FadingIn;
            }
            if player.end_secs > 0.0 && player.fade_out_secs > 0.0 {
                let remaining = (player.end_secs - pos).max(0.0);
                if remaining < player.fade_out_secs {
                    let t = (remaining / player.fade_out_secs) as f32;
                    env *= player
                        .fade_curve
                        .apply_with_points(t, &player.custom_fade_points);
                    state = CueState::FadingOut;
                }
            }

            if let Some(ref fade) = player.dynamic_fade {
                let elapsed = fade.start_time.elapsed().as_secs_f64();
                if elapsed >= fade.duration_secs {
                    player.base_volume = fade.target_vol;
                    let should_stop = fade.stop_on_finish;
                    player.dynamic_fade = None;
                    if should_stop {
                        player.sink.stop();
                        finished_ids.push(player.id);
                        continue;
                    }
                } else {
                    let t = (elapsed / fade.duration_secs.max(0.001)) as f32;
                    let shaped = fade.curve.apply_with_points(t, &fade.custom_fade_points);
                    let interpolated = fade.start_vol + (fade.target_vol - fade.start_vol) * shaped;
                    player.current_volume = interpolated * env;
                    player.sink.set_volume(player.current_volume);
                    state = if fade.target_vol >= fade.start_vol {
                        CueState::FadingIn
                    } else {
                        CueState::FadingOut
                    };
                    let _ = pos_tx.try_send(PositionUpdate {
                        id: player.id,
                        position_secs: pos,
                        state,
                    });
                    continue;
                }
            }

            player.current_volume = player.base_volume * env;
            player.sink.set_volume(player.current_volume);
            let _ = pos_tx.try_send(PositionUpdate {
                id: player.id,
                position_secs: pos,
                state,
            });
        }

        for finished_id in finished_ids {
            players.retain(|player| player.id != finished_id);
            let _ = pos_tx.try_send(PositionUpdate {
                id: finished_id,
                position_secs: 0.0,
                state: CueState::Stopped,
            });
        }

        std::thread::sleep(tick);
    }
}

fn handle_command(
    cmd: AudioCommand,
    players: &mut Vec<ActivePlayer>,
    streams: &mut HashMap<String, DeviceOutput>,
    host: &Host,
    default_device_name: &str,
    default_settings: &Arc<RwLock<Vec<OutputChannelSetting>>>,
) {
    match cmd {
        AudioCommand::Play {
            id,
            file,
            output_device,
            start_secs,
            end_secs,
            duration_secs,
            volume,
            fade_in_secs,
            fade_out_secs,
            fade_curve,
            custom_fade_points,
            channel_routes,
            loop_enabled,
            loop_start,
            loop_end,
            loop_count,
        } => {
            let prepare_started = Instant::now();
            players.retain(|player| {
                if player.id == id {
                    player.sink.stop();
                    false
                } else {
                    true
                }
            });

            let uses_global_default = output_device.is_none();
            let device_name = output_device.unwrap_or_else(|| default_device_name.to_string());
            let Some((stream_handle, output_channels, output_settings)) = ensure_device_output(
                streams,
                &device_name,
                host,
                default_device_name,
                default_settings,
            ) else {
                warn!("Cannot open output device '{}' for cue {}", device_name, id);
                return;
            };

            let initial_volume = if fade_in_secs > 0.0 { 0.0 } else { volume };
            let route_state = Arc::new(RwLock::new(channel_routes));
            let Some(sink) = open_sink_at(
                stream_handle.as_ref(),
                &file,
                start_secs,
                initial_volume,
                output_channels,
                route_state.clone(),
                output_settings.clone(),
            ) else {
                warn!("Cannot open audio file for playback: {:?}", file);
                return;
            };
            info!(
                "Cue {} output device + decoder/sink prepared in {:.1} ms (device: {})",
                id,
                prepare_started.elapsed().as_secs_f64() * 1000.0,
                device_name
            );

            let effective_end = end_secs
                .unwrap_or(duration_secs)
                .min(if duration_secs > 0.0 {
                    duration_secs
                } else {
                    f64::MAX
                });
            let effective_loop_end = if loop_end > loop_start {
                loop_end
            } else {
                effective_end
            };

            players.push(ActivePlayer {
                id,
                file,
                sink,
                start_secs,
                end_secs: if effective_end == f64::MAX {
                    0.0
                } else {
                    effective_end
                },
                base_volume: volume,
                current_volume: initial_volume,
                fade_in_secs,
                fade_out_secs,
                fade_curve,
                custom_fade_points,
                channel_routes: route_state,
                output_channels,
                output_device: device_name,
                uses_global_default,
                stream_handle,
                output_settings,
                loop_enabled,
                loop_start,
                loop_end: effective_loop_end,
                loops_remaining: loop_count,
                segment_start_pos: start_secs,
                segment_instant: Instant::now(),
                paused_at: None,
                dynamic_fade: None,
            });
        }

        AudioCommand::Stop(id) => {
            if let Some(player) = players.iter().find(|player| player.id == id) {
                player.sink.stop();
            }
            players.retain(|player| player.id != id);
        }

        AudioCommand::StopAll => {
            for player in players.iter() {
                player.sink.stop();
            }
            players.clear();
        }

        AudioCommand::Pause(id) => {
            if let Some(player) = players.iter_mut().find(|player| player.id == id) {
                let position = player.current_pos();
                player.sink.pause();
                player.paused_at = Some(position);
            }
        }

        AudioCommand::Seek { id, position_secs } => {
            if let Some(player) = players.iter_mut().find(|player| player.id == id) {
                let min_position = player.start_secs.max(0.0);
                let max_position = if player.end_secs > 0.0 {
                    (player.end_secs - 0.01).max(min_position)
                } else {
                    f64::MAX
                };
                let position = position_secs.clamp(min_position, max_position);
                player.sink.stop();
                if let Some(new_sink) = open_sink_at(
                    player.stream_handle.as_ref(),
                    &player.file,
                    position,
                    player.current_volume,
                    player.output_channels,
                    player.channel_routes.clone(),
                    player.output_settings.clone(),
                ) {
                    new_sink.pause();
                    player.sink = new_sink;
                    player.segment_start_pos = position;
                    player.segment_instant = Instant::now();
                    player.paused_at = Some(position);
                    player.dynamic_fade = None;
                } else {
                    warn!("Could not seek audio cue {} to {:.3}s", id, position);
                }
            }
        }

        AudioCommand::PauseAll => {
            for player in players.iter_mut() {
                let position = player.current_pos();
                player.sink.pause();
                player.paused_at = Some(position);
            }
        }

        AudioCommand::Resume(id) => {
            if let Some(player) = players.iter_mut().find(|player| player.id == id) {
                if let Some(position) = player.paused_at.take() {
                    player.segment_start_pos = position;
                    player.segment_instant = Instant::now();
                }
                player.sink.play();
            }
        }

        AudioCommand::ResumeAll => {
            for player in players.iter_mut() {
                if let Some(position) = player.paused_at.take() {
                    player.segment_start_pos = position;
                    player.segment_instant = Instant::now();
                }
                player.sink.play();
            }
        }

        AudioCommand::SetVolume(id, volume) => {
            if let Some(player) = players.iter_mut().find(|player| player.id == id) {
                player.base_volume = volume;
                player.sink.set_volume(volume);
            }
        }

        AudioCommand::Fade {
            id,
            target_volume,
            duration_secs,
            stop_on_finish,
            curve,
            custom_fade_points,
        } => {
            if let Some(player) = players.iter_mut().find(|player| player.id == id) {
                player.dynamic_fade = Some(DynamicFade {
                    start_vol: player.current_volume,
                    target_vol: target_volume,
                    start_time: Instant::now(),
                    duration_secs,
                    stop_on_finish,
                    curve,
                    custom_fade_points,
                });
            }
        }

        AudioCommand::SetRouting { id, routes } => {
            if let Some(player) = players.iter_mut().find(|player| player.id == id) {
                *player.channel_routes.write() = routes;
            }
        }

        AudioCommand::SetOutputDevice(_) | AudioCommand::SetInputDevice(_) => {}
    }
}

#[cfg(test)]
mod channel_router_tests {
    use super::*;
    use rodio::buffer::SamplesBuffer;

    #[test]
    fn track_routes_map_source_channels_to_selected_hardware_outputs() {
        let routes = Arc::new(RwLock::new(vec![
            ChannelRoute {
                active: true,
                output_channel: 2,
                gain_db: 0.0,
            },
            ChannelRoute {
                active: true,
                output_channel: 3,
                gain_db: 0.0,
            },
        ]));
        let output_settings = Arc::new(RwLock::new(vec![OutputChannelSetting::default(); 4]));
        let source = SamplesBuffer::new(2, 48_000, vec![0.25_f32, 0.75_f32]);
        let router = ChannelRouter::new(source, routes, output_settings, 4);
        let output: Vec<f32> = router.take(4).collect();

        assert_eq!(output, vec![0.0, 0.0, 0.25, 0.75]);
    }

    #[test]
    fn inactive_track_source_channel_is_not_sent() {
        let routes = Arc::new(RwLock::new(vec![
            ChannelRoute {
                active: false,
                output_channel: 0,
                gain_db: 0.0,
            },
            ChannelRoute {
                active: true,
                output_channel: 1,
                gain_db: 0.0,
            },
        ]));
        let output_settings = Arc::new(RwLock::new(vec![OutputChannelSetting::default(); 2]));
        let source = SamplesBuffer::new(2, 48_000, vec![0.5_f32, 0.75_f32]);
        let router = ChannelRouter::new(source, routes, output_settings, 2);
        let output: Vec<f32> = router.take(2).collect();

        assert_eq!(output, vec![0.0, 0.75]);
    }
}
