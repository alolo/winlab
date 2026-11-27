// src/audio/engine.rs — аудиодвижок с поддержкой Fade In/Out, Start/End, Loop и смены I/O карт

use crate::audio::device_registry::{
    AudioEndpoint, DeviceAvailability, DeviceRegistry, DeviceScanResult,
};
use crate::audio::worker::{
    run_audio_worker_manager, spawn_audio_worker, AudioWorkerJobSupervisor,
    AudioWorkerManagerEvent, WorkerEngineMetadata, WorkerStartupConfig, WORKER_PROTOCOL_VERSION,
};
use crate::audio::OutputChannelInfo;
use crate::config::{AudioBackend, AudioPerformanceSettings};
use crate::cue::{ChannelRoute, CueState, FadeCurve, OutputChannelSetting, TimePitchSettings};
use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Host, Sample};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use parking_lot::RwLock;
use rodio::Source;
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use std::collections::{HashMap, VecDeque};
#[cfg(target_os = "windows")]
use std::ffi::{CStr, CString};
#[cfg(target_os = "windows")]
use std::mem::MaybeUninit;
use std::path::PathBuf;
#[cfg(target_os = "windows")]
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

fn unix_micros_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(u64::MAX as u128) as u64
}

/// Store a floating-point playback position without taking a lock in the audio
/// source's `next()` path. The position is observational state, so relaxed
/// ordering is sufficient; no other data is published through this atomic.
#[inline]
fn atomic_store_f64(target: &AtomicU64, value: f64) {
    target.store(value.to_bits(), Ordering::Relaxed);
}

/// Read a playback position published by an audio source.
#[inline]
fn atomic_load_f64(source: &AtomicU64) -> f64 {
    f64::from_bits(source.load(Ordering::Relaxed))
}

type SharedOutputSettings = Arc<RwLock<Vec<OutputChannelSetting>>>;

/// Sample one ChannelRouter frame out of every N frames for low-overhead profiling.
const CHANNEL_ROUTER_PROFILE_STRIDE: u64 = 1_024;
/// Sample one complete endpoint-mix frame out of every N frames.
const MIXER_OUTPUT_PROFILE_STRIDE: u64 = 1_024;
type OutputSettingsByDevice = Arc<RwLock<HashMap<String, SharedOutputSettings>>>;
const AUDIO_MANAGER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const AUDIO_MANAGER_FORCE_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// Playback payload shared by a single Worker batch. Endpoint selection is
/// already resolved by the parent Worker Manager, so every cue in this batch
/// belongs to the same native output stream.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AudioPlayRequest {
    pub id: Uuid,
    pub generation: u64,
    pub file: PathBuf,
    pub start_secs: f64,
    pub end_secs: Option<f64>,
    pub duration_secs: f64,
    pub volume: f32,
    pub volume_automation_enabled: bool,
    pub volume_automation: Vec<[f64; 2]>,
    pub time_pitch: TimePitchSettings,
    pub fade_in_secs: f64,
    pub fade_out_secs: f64,
    pub fade_curve: FadeCurve,
    pub custom_fade_points: Vec<[f32; 2]>,
    pub channel_routes: Vec<ChannelRoute>,
    pub output_gains_db: Vec<f32>,
    pub loop_enabled: bool,
    pub loop_start: f64,
    pub loop_end: f64,
    pub loop_count: i32,
}

#[derive(Debug, Clone)]
pub enum AudioCommand {
    Play {
        id: Uuid,
        /// Monotonic per-cue generation used to discard stale position updates after Stop/restart.
        generation: u64,
        file: PathBuf,
        /// None uses the global output device; Some selects a per-cue device.
        output_device: Option<String>,
        /// Backend and stable identity for an explicit per-Cue endpoint.
        output_backend: Option<AudioBackend>,
        output_device_id: Option<String>,
        start_secs: f64,
        end_secs: Option<f64>,
        duration_secs: f64,
        volume: f32,
        volume_automation_enabled: bool,
        volume_automation: Vec<[f64; 2]>,
        time_pitch: TimePitchSettings,
        fade_in_secs: f64,
        fade_out_secs: f64,
        fade_curve: FadeCurve,
        custom_fade_points: Vec<[f32; 2]>,
        channel_routes: Vec<ChannelRoute>,
        output_gains_db: Vec<f32>,
        loop_enabled: bool,
        loop_start: f64,
        loop_end: f64,
        loop_count: i32, // -1 = бесконечный, >0 = общее число проходов вместе с первым
    },
    /// Internal child-Worker command: prepare several cues and arm them against
    /// one output sample-frame clock. A batch is created from an explicit launch
    /// transaction, never by guessing from the arrival-time distance between
    /// independent Play commands.
    PlayBatch {
        transaction_id: Uuid,
        cues: Vec<AudioPlayRequest>,
    },
    /// GUI -> Worker Manager boundary for one logical GO/Auto-Continue launch.
    /// The manager forwards these markers only to endpoint workers that receive
    /// at least one Play command inside this transaction.
    BeginLaunchTransaction {
        transaction_id: Uuid,
    },
    EndLaunchTransaction {
        transaction_id: Uuid,
    },
    Stop(Uuid),
    StopTimed {
        id: Uuid,
        request_id: Uuid,
        requested_at_unix_micros: u64,
    },
    StopAll,
    StopAllTimed {
        request_id: Uuid,
        requested_at_unix_micros: u64,
    },
    Pause(Uuid),
    Seek {
        id: Uuid,
        position_secs: f64,
    },
    PauseAll,
    Resume(Uuid),
    ResumeAll,
    SetVolume(Uuid, f32),
    SetVolumeAutomation {
        id: Uuid,
        enabled: bool,
        points: Vec<[f64; 2]>,
    },
    SetTimePitch {
        id: Uuid,
        settings: TimePitchSettings,
    },
    Fade {
        id: Uuid,
        target_volume: f32,
        duration_secs: f64,
        stop_on_finish: bool,
        curve: FadeCurve,
        custom_fade_points: Vec<[f32; 2]>,
    },
    SetOutputDevice {
        device_name: String,
        endpoint_id: Option<String>,
        performance: AudioPerformanceSettings,
        reply: Sender<Result<(usize, u32), String>>,
    },
    ReconfigureOutput {
        performance: AudioPerformanceSettings,
        reply: Sender<Result<(usize, u32), String>>,
    },
    SetInputDevice(String),
    /// Play a raw multichannel test tone through one native stream channel.
    /// logical_channel is the user-facing Theatre Show Cue output slot; channel is the
    /// zero-based native CPAL/ASIO stream index selected by that slot.
    TestOutput {
        device_name: String,
        logical_channel: usize,
        channel: usize,
    },
    SetRouting {
        id: Uuid,
        routes: Vec<ChannelRoute>,
    },
    SetOutputGains {
        id: Uuid,
        gains_db: Vec<f32>,
    },
    SetOutputChannelSettings {
        device_name: String,
        settings: Vec<OutputChannelSetting>,
    },
    /// Manager-only snapshot of workstation-wide endpoint preferences. The
    /// manager applies the matching profile whenever it starts an endpoint worker.
    SetEndpointPreferences {
        performance_settings_by_device: HashMap<String, AudioPerformanceSettings>,
        output_channel_settings_by_device: HashMap<String, Vec<OutputChannelSetting>>,
    },
    /// Fresh endpoint availability snapshot from the background hot-plug monitor.
    UpdateDeviceRegistry {
        endpoints: Vec<AudioEndpoint>,
    },
    Shutdown,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PositionUpdate {
    pub id: Uuid,
    pub generation: u64,
    pub position_secs: f64,
    pub state: CueState,
}

fn host_for_backend(backend: AudioBackend) -> Result<Host> {
    #[cfg(target_os = "windows")]
    {
        return match backend {
            // CPAL's Windows default host is WASAPI even when the ASIO feature
            // is compiled in. Use that default directly so enabling ASIO never
            // changes or hides the normal shared-mode WASAPI path.
            AudioBackend::Wasapi => Ok(cpal::default_host()),
            AudioBackend::Asio => cpal::host_from_id(cpal::HostId::Asio).map_err(|error| {
                anyhow::anyhow!("Could not initialise ASIO audio host: {}", error)
            }),
            AudioBackend::DirectSound => {
                anyhow::bail!("DirectSound uses the SDL2 output path, not the CPAL host")
            }
        };
    }
    #[cfg(not(target_os = "windows"))]
    {
        match backend {
            AudioBackend::Wasapi => Ok(cpal::default_host()),
            AudioBackend::Asio => Err(anyhow::anyhow!("ASIO backend is only available on Windows")),
            AudioBackend::DirectSound => Err(anyhow::anyhow!(
                "DirectSound backend is only available on Windows"
            )),
        }
    }
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

const COMMON_SAMPLE_RATES: &[u32] = &[44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
const COMMON_BUFFER_SIZES: &[u32] = &[32, 64, 128, 256, 512, 1_024, 2_048, 4_096, 8_192];
/// WASAPI Shared accepts an application-requested buffer duration independently
/// of the Windows audio-engine callback period.
const WASAPI_SHARED_BUFFER_SIZES: &[u32] = &[256, 512, 1_024, 2_048, 4_096, 8_192];
/// DirectSound is fed by TaSQ's SDL queue, so these are real application queue targets.
const DIRECTSOUND_BUFFER_SIZES: &[u32] = &[1_024, 2_048, 4_096, 8_192, 16_384];

fn buffer_size_options_for_backend(
    backend: AudioBackend,
    driver_reported_sizes: &[u32],
) -> Vec<u32> {
    match backend {
        AudioBackend::Asio => driver_reported_sizes.to_vec(),
        AudioBackend::Wasapi => WASAPI_SHARED_BUFFER_SIZES.to_vec(),
        AudioBackend::DirectSound => DIRECTSOUND_BUFFER_SIZES.to_vec(),
    }
}

fn supported_output_sample_rates(device: &Device) -> Vec<u32> {
    let Ok(configs) = device.supported_output_configs() else {
        return device
            .default_output_config()
            .ok()
            .map(|config| vec![config.sample_rate()])
            .unwrap_or_default();
    };
    let configs: Vec<_> = configs.collect();
    let Some(max_channels) = configs.iter().map(|config| config.channels()).max() else {
        return Vec::new();
    };

    let mut rates = Vec::new();
    for config in configs
        .iter()
        .filter(|config| config.channels() == max_channels)
    {
        for rate in COMMON_SAMPLE_RATES {
            if config.contains_rate(*rate) && !rates.contains(rate) {
                rates.push(*rate);
            }
        }
    }
    if let Ok(default_config) = device.default_output_config() {
        let rate = default_config.sample_rate();
        if default_config.channels() == max_channels && !rates.contains(&rate) {
            rates.push(rate);
        }
    }
    rates.sort_unstable();
    rates
}

fn supported_output_buffer_sizes(device: &Device, backend: AudioBackend) -> Vec<u32> {
    let fallback = if backend == AudioBackend::Asio {
        Vec::new()
    } else {
        COMMON_BUFFER_SIZES.to_vec()
    };
    let Ok(configs) = device.supported_output_configs() else {
        return fallback;
    };
    let configs: Vec<_> = configs.collect();
    let Some(max_channels) = configs.iter().map(|config| config.channels()).max() else {
        return fallback;
    };

    let mut sizes = Vec::new();
    let mut unknown_range = false;
    for config in configs
        .iter()
        .filter(|config| config.channels() == max_channels)
    {
        match config.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } => {
                for size in COMMON_BUFFER_SIZES {
                    if *size >= *min && *size <= *max && !sizes.contains(size) {
                        sizes.push(*size);
                    }
                }
            }
            cpal::SupportedBufferSize::Unknown => unknown_range = true,
        }
    }
    if unknown_range || sizes.is_empty() {
        return fallback;
    }
    sizes.sort_unstable();
    sizes
}

fn preferred_output_config(
    backend: AudioBackend,
    device: &Device,
    requested_sample_rate: Option<u32>,
) -> Option<cpal::SupportedStreamConfig> {
    // WASAPI Shared follows the Windows endpoint mix format unless the user
    // explicitly selects another supported rate. ASIO always follows the
    // driver's currently selected hardware clock and never accepts an override.
    if backend == AudioBackend::Wasapi {
        if let Ok(default_config) = device.default_output_config() {
            match requested_sample_rate {
                None => return Some(default_config),
                Some(rate) if default_config.sample_rate() == rate => {
                    return Some(default_config);
                }
                Some(rate) => {
                    let default_channels = default_config.channels();
                    if let Ok(configs) = device.supported_output_configs() {
                        if let Some(config) = configs
                            .filter(|config| {
                                config.channels() == default_channels && config.contains_rate(rate)
                            })
                            .map(|config| config.with_sample_rate(rate))
                            .next()
                        {
                            return Some(config);
                        }
                    }
                }
            }
        }
    }

    if backend == AudioBackend::Asio {
        let default_config = device.default_output_config().ok()?;
        let driver_rate = default_config.sample_rate();
        let preferred_format = default_config.sample_format();
        debug!(
            "ASIO driver current clock: {} Hz ({:?}); TaSQ performance overrides are ignored",
            driver_rate, preferred_format
        );
        if let Ok(configs) = device.supported_output_configs() {
            if let Some(config) = configs
                .filter(|config| config.contains_rate(driver_rate))
                .max_by_key(|config| {
                    (
                        config.channels(),
                        (config.sample_format() == preferred_format) as u8,
                        (config.sample_format() == cpal::SampleFormat::F32) as u8,
                    )
                })
            {
                return Some(config.with_sample_rate(driver_rate));
            }
        }
        return Some(default_config);
    }

    let configs: Vec<_> = device.supported_output_configs().ok()?.collect();
    let max_channels = configs.iter().map(|config| config.channels()).max()?;
    let config = configs
        .into_iter()
        .filter(|config| config.channels() == max_channels)
        .filter(|config| requested_sample_rate.map_or(true, |rate| config.contains_rate(rate)))
        .max_by_key(|config| {
            let min_rate = config.min_sample_rate();
            let max_rate = config.max_sample_rate();
            let rate_priority = if requested_sample_rate.is_some() {
                4_u8
            } else if min_rate <= 48_000 && max_rate >= 48_000 {
                3_u8
            } else if min_rate <= 44_100 && max_rate >= 44_100 {
                2_u8
            } else if min_rate <= 96_000 && max_rate >= 96_000 {
                1_u8
            } else {
                0_u8
            };
            (
                config.channels(),
                rate_priority,
                (config.sample_format() == cpal::SampleFormat::F32) as u8,
            )
        })?;

    let sample_rate = requested_sample_rate.unwrap_or_else(|| {
        let min_rate = config.min_sample_rate();
        let max_rate = config.max_sample_rate();
        if min_rate <= 48_000 && max_rate >= 48_000 {
            48_000
        } else if min_rate <= 44_100 && max_rate >= 44_100 {
            44_100
        } else {
            max_rate
        }
    });
    Some(config.with_sample_rate(sample_rate))
}

pub struct AudioEngine {
    pub backend: AudioBackend,
    pub cmd_tx: Sender<AudioCommand>,
    pub pos_rx: Receiver<PositionUpdate>,
    pub output_devices: Vec<String>,
    /// Central cross-backend output endpoint registry, including offline devices.
    pub device_registry: DeviceRegistry,
    pub input_devices: Vec<String>,
    pub current_device: String,
    /// Stable backend-provided identity for the active global output endpoint.
    pub current_device_id: String,
    pub current_input_device: String,
    pub output_channels: usize,
    pub current_sample_rate: u32,
    pub performance_settings: AudioPerformanceSettings,
    pub supported_sample_rates: Vec<u32>,
    pub supported_buffer_sizes: Vec<u32>,
    output_device_channels: HashMap<String, usize>,
    output_device_channel_layouts: HashMap<String, Vec<usize>>,
    /// Per-device output mute/trim settings shared with the active backend worker.
    output_settings_by_device: OutputSettingsByDevice,
    /// Workstation output settings for endpoints owned by other backend workers.
    /// The manager forwards these settings when a per-Cue worker is created.
    endpoint_output_channel_settings: Arc<RwLock<HashMap<String, Vec<OutputChannelSetting>>>>,
    /// Optional channel names reported by the native ASIO driver, keyed by CPAL device name.
    output_driver_channel_info: HashMap<String, Vec<OutputChannelInfo>>,
    /// User-defined aliases override driver-provided names but never change routing indices.
    output_channel_names: HashMap<String, Vec<String>>,
    default_output_pairs: HashMap<String, [usize; 2]>,
    /// Logical Theatre Show Cue output order -> native CPAL/ASIO stream channel index.
    /// Stored per device so non-standard driver orders do not affect other interfaces.
    output_channel_orders: HashMap<String, Vec<usize>>,
    detected_default_output_pairs: HashMap<String, [usize; 2]>,
    /// Present only inside an audio child process; joins the in-process render thread on shutdown.
    audio_thread: Option<std::thread::JoinHandle<()>>,
    /// Present only in the GUI process; routes commands across endpoint workers.
    worker_manager_thread: Option<std::thread::JoinHandle<()>>,
    worker_manager_shutdown: Option<Arc<AtomicBool>>,
    /// Manager-to-GUI notifications for endpoint loss and asynchronous recovery.
    worker_event_rx: Option<Receiver<AudioWorkerManagerEvent>>,
    /// Lock-free sample-peak accumulator updated by each output mix callback.
    /// In worker-child mode the IPC loop drains it at 10 Hz for the GUI meter.
    output_peak_meter: Arc<AtomicU32>,
    /// Supervises isolated worker processes and can terminate them if manager shutdown stalls.
    worker_job_supervisor: Option<Arc<AudioWorkerJobSupervisor>>,
}

impl AudioEngine {
    pub fn new() -> Result<Self> {
        Self::new_with_backend(AudioBackend::Wasapi)
    }

    pub fn new_with_backend(backend: AudioBackend) -> Result<Self> {
        Self::new_with_backend_and_device(backend, None)
    }

    /// Initialize the selected backend and, when supplied, open the exact
    /// requested endpoint before the audio worker starts. This avoids briefly
    /// opening a different ASIO driver during startup/device changes.
    pub fn new_with_backend_and_device(
        backend: AudioBackend,
        selected_device: Option<&str>,
    ) -> Result<Self> {
        Self::new_with_backend_and_endpoint(backend, None, selected_device)
    }

    /// Initialize a backend using a stable endpoint ID first, then the saved
    /// display name as a migration/fallback path for older settings files.
    pub fn new_with_backend_and_endpoint(
        backend: AudioBackend,
        selected_device_id: Option<&str>,
        selected_device: Option<&str>,
    ) -> Result<Self> {
        Self::new_with_backend_and_endpoint_and_performance(
            backend,
            selected_device_id,
            selected_device,
            AudioPerformanceSettings::default(),
        )
    }

    pub fn new_with_backend_and_endpoint_and_performance(
        backend: AudioBackend,
        selected_device_id: Option<&str>,
        selected_device: Option<&str>,
        performance: AudioPerformanceSettings,
    ) -> Result<Self> {
        let performance = AudioPerformanceSettings::for_backend(backend, performance);
        let device_registry = DeviceRegistry::from_scan(Self::scan_output_endpoints());
        Self::new_with_backend_and_endpoint_using_registry_and_performance(
            backend,
            selected_device_id,
            selected_device,
            device_registry,
            performance,
        )
    }

    pub fn new_with_backend_and_endpoint_using_registry_and_performance(
        backend: AudioBackend,
        selected_device_id: Option<&str>,
        selected_device: Option<&str>,
        device_registry: DeviceRegistry,
        performance: AudioPerformanceSettings,
    ) -> Result<Self> {
        let performance = AudioPerformanceSettings::for_backend(backend, performance);
        let config = WorkerStartupConfig {
            protocol_version: WORKER_PROTOCOL_VERSION,
            backend,
            selected_device_id: selected_device_id.map(str::to_owned),
            selected_device: selected_device.map(str::to_owned),
            performance,
            include_full_device_metadata: true,
            registry_endpoints: Some(device_registry.snapshot_endpoints()),
        };
        let job_supervisor = AudioWorkerJobSupervisor::new();
        let spawned = spawn_audio_worker(config, job_supervisor.clone())?;
        let metadata = spawned.metadata.clone();

        let registry = if metadata.registry_endpoints.is_empty() {
            device_registry
        } else {
            DeviceRegistry::from_endpoints(metadata.registry_endpoints.clone())
        };

        let mut output_device_channels = metadata.output_device_channels.clone();
        output_device_channels
            .entry(metadata.current_device.clone())
            .or_insert(metadata.output_channels.max(1));
        let mut output_device_channel_layouts = metadata.output_device_channel_layouts.clone();
        output_device_channel_layouts
            .entry(metadata.current_device.clone())
            .or_insert_with(|| vec![metadata.output_channels.max(1)]);

        let output_settings_by_device = Arc::new(RwLock::new(HashMap::new()));
        for (device_name, channels) in &output_device_channels {
            let _ =
                shared_output_settings(&output_settings_by_device, device_name, (*channels).max(1));
        }

        // Driver initialization can take several seconds. An unbounded control
        // queue prevents a slow worker startup from blocking the GUI thread.
        let (manager_command_tx, manager_command_rx) = unbounded::<AudioCommand>();
        let (manager_position_tx, manager_position_rx) = bounded::<PositionUpdate>(4096);
        let manager_registry_endpoints = registry.snapshot_endpoints();
        let manager_shutdown = Arc::new(AtomicBool::new(false));
        let manager_shutdown_thread = manager_shutdown.clone();
        let manager_job_supervisor = job_supervisor.clone();
        let (manager_event_tx, manager_event_rx) = unbounded::<AudioWorkerManagerEvent>();
        let manager_thread = std::thread::Builder::new()
            .name("tasq-audio-worker-manager".to_string())
            .spawn(move || {
                run_audio_worker_manager(
                    spawned,
                    manager_command_rx,
                    manager_position_tx,
                    manager_registry_endpoints,
                    manager_shutdown_thread,
                    manager_job_supervisor,
                    manager_event_tx,
                );
            })
            .map_err(|error| anyhow::anyhow!("Could not start audio worker manager: {error}"))?;

        Ok(Self {
            backend: metadata.backend,
            cmd_tx: manager_command_tx,
            pos_rx: manager_position_rx,
            output_devices: metadata.output_devices,
            device_registry: registry,
            input_devices: metadata.input_devices,
            current_device: metadata.current_device,
            current_device_id: metadata.current_device_id,
            current_input_device: metadata.current_input_device,
            output_channels: metadata.output_channels,
            current_sample_rate: metadata.current_sample_rate,
            performance_settings: metadata.performance_settings,
            supported_sample_rates: metadata.supported_sample_rates,
            supported_buffer_sizes: metadata.supported_buffer_sizes,
            output_device_channels,
            output_device_channel_layouts,
            output_settings_by_device,
            endpoint_output_channel_settings: Arc::new(RwLock::new(HashMap::new())),
            output_driver_channel_info: metadata.output_driver_channel_info,
            output_channel_names: HashMap::new(),
            default_output_pairs: HashMap::new(),
            output_channel_orders: HashMap::new(),
            detected_default_output_pairs: metadata.detected_default_output_pairs,
            audio_thread: None,
            worker_manager_thread: Some(manager_thread),
            worker_manager_shutdown: Some(manager_shutdown),
            worker_event_rx: Some(manager_event_rx),
            output_peak_meter: Arc::new(AtomicU32::new(0)),
            worker_job_supervisor: Some(job_supervisor),
        })
    }

    pub(crate) fn new_in_process_with_backend_and_endpoint_using_registry_and_performance(
        backend: AudioBackend,
        selected_device_id: Option<&str>,
        selected_device: Option<&str>,
        mut device_registry: DeviceRegistry,
        performance: AudioPerformanceSettings,
        include_full_device_metadata: bool,
    ) -> Result<Self> {
        let performance = AudioPerformanceSettings::for_backend(backend, performance);
        if backend == AudioBackend::DirectSound {
            #[cfg(target_os = "windows")]
            {
                let selected_endpoint = selected_device_id
                    .and_then(|id| device_registry.find_available_by_id(id))
                    .filter(|endpoint| endpoint.backend == AudioBackend::DirectSound)
                    .cloned()
                    .or_else(|| {
                        selected_device.and_then(|name| {
                            device_registry
                                .find_available_by_backend_name(AudioBackend::DirectSound, name)
                                .cloned()
                        })
                    });
                let selected_directsound_name = selected_endpoint
                    .as_ref()
                    .map(|endpoint| endpoint.device_name.clone())
                    .or_else(|| selected_device.map(str::to_string));
                let mut engine = Self::new_directsound(
                    selected_directsound_name,
                    selected_endpoint,
                    performance,
                    include_full_device_metadata,
                )?;
                engine.device_registry = device_registry;
                for device_name in &engine.output_devices {
                    let channels = engine
                        .output_device_channels
                        .get(device_name)
                        .copied()
                        .unwrap_or(2)
                        .max(1);
                    let channel_layouts = engine
                        .output_device_channel_layouts
                        .get(device_name)
                        .cloned()
                        .unwrap_or_else(|| vec![channels]);
                    engine.device_registry.upsert_available(AudioEndpoint {
                        id: Self::fallback_endpoint_id(AudioBackend::DirectSound, device_name),
                        backend: AudioBackend::DirectSound,
                        device_name: device_name.clone(),
                        channels,
                        channel_layouts,
                        availability: DeviceAvailability::Available,
                    });
                }
                engine.current_device_id =
                    Self::fallback_endpoint_id(AudioBackend::DirectSound, &engine.current_device);
                return Ok(engine);
            }
            #[cfg(not(target_os = "windows"))]
            {
                anyhow::bail!("DirectSound backend is only available on Windows");
            }
        }

        let endpoint_init_started = Instant::now();
        let host = host_for_backend(backend).map_err(|error| {
            error!(
                "Could not initialize {} audio host: {error:#}",
                backend.label()
            );
            error
        })?;
        let out_dev = if let Some(selected_id) = selected_device_id {
            let parsed_id = selected_id.parse::<cpal::DeviceId>().ok();
            let by_id = parsed_id
                .as_ref()
                .and_then(|device_id| host.device_by_id(device_id));
            if let Some(device) = by_id {
                device
            } else if !include_full_device_metadata {
                // Per-Cue Workers are created from GO. Never enumerate devices
                // on this path: use the cached stable ID, or accept the cached
                // default endpoint by name, otherwise fail fast and ask for a
                // registry refresh instead of blocking the launch transaction.
                let default_device = host.default_output_device();
                match (default_device, selected_device) {
                    (Some(device), Some(selected_name))
                        if device.to_string() == selected_name => device,
                    _ => anyhow::bail!(
                        "Selected {} endpoint ID '{}' could not be resolved from the cached device ID without scanning devices. Refresh the device registry or reselect the output.",
                        backend.label(),
                        selected_id
                    ),
                }
            } else if let Some(selected_name) = selected_device {
                host.output_devices()?
                    .find(|device| device.to_string() == selected_name)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Selected {} endpoint ID '{}' and device name '{}' are no longer available",
                            backend.label(),
                            selected_id,
                            selected_name
                        )
                    })?
            } else {
                anyhow::bail!(
                    "Selected {} endpoint ID '{}' is no longer available",
                    backend.label(),
                    selected_id
                );
            }
        } else if let Some(selected_name) = selected_device {
            if !include_full_device_metadata {
                host.default_output_device()
                    .filter(|device| device.to_string() == selected_name)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Selected {} endpoint '{}' has no stable cached ID and is not the current default output; refusing to scan devices during GO",
                            backend.label(),
                            selected_name
                        )
                    })?
            } else {
                host.output_devices()?
                    .find(|device| device.to_string() == selected_name)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Selected {} output device '{}' is no longer available",
                            backend.label(),
                            selected_name
                        )
                    })?
            }
        } else {
            host.default_output_device()
                .or_else(|| {
                    include_full_device_metadata
                        .then(|| {
                            host.output_devices()
                                .ok()
                                .and_then(|mut devices| devices.next())
                        })
                        .flatten()
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("No output audio device found for {}", backend.label())
                })?
        };
        // CPAL 0.18 removed DeviceTrait::name(); Display returns the user-facing device name.
        let out_name = out_dev.to_string();
        let out_id = Self::cpal_endpoint_id(backend, &out_dev, &out_name);

        // A per-Cue isolated worker owns one endpoint only. Enumerating every
        // ASIO input/output port in that process can take several seconds and
        // delays the first GO. The global worker still collects the complete
        // inventory for the settings UI.
        let output_devices = if include_full_device_metadata {
            Self::list_output_devices(&host)
        } else {
            vec![out_name.clone()]
        };
        info!(
            "Enumerated {} output device(s) for {} (full metadata={}): {:?}",
            output_devices.len(),
            backend.label(),
            include_full_device_metadata,
            output_devices
        );

        // No vendor-specific ASIO probes or hardcoded Computer Bus mapping.
        // CPAL's stream order is authoritative; generic channel labels are
        // used until a backend exposes driver-native names directly.
        let output_driver_channel_info: HashMap<String, Vec<OutputChannelInfo>> = HashMap::new();
        let (mut output_device_channels, mut output_device_channel_layouts) =
            if include_full_device_metadata {
                Self::list_output_device_info(&host)
            } else {
                (HashMap::new(), HashMap::new())
            };

        let output_config = preferred_output_config(backend, &out_dev, performance.sample_rate)
            .or_else(|| {
                out_dev.default_output_config().ok().filter(|config| {
                    performance
                        .sample_rate
                        .map_or(true, |rate| config.sample_rate() == rate)
                })
            })
            .ok_or_else(|| {
                if let Some(rate) = performance.sample_rate {
                    anyhow::anyhow!(
                        "{} does not expose a supported output configuration at {} Hz",
                        out_name,
                        rate
                    )
                } else {
                    anyhow::anyhow!("No supported output configuration found for {}", out_name)
                }
            })?;
        let preferred_output_channels = output_config.channels() as usize;
        if !include_full_device_metadata {
            output_device_channels.insert(out_name.clone(), preferred_output_channels.max(1));
            output_device_channel_layouts
                .insert(out_name.clone(), vec![preferred_output_channels.max(1)]);
        }

        for device_name in &output_devices {
            let channels = output_device_channels
                .get(device_name)
                .copied()
                .unwrap_or(2)
                .max(1);
            let channel_layouts = output_device_channel_layouts
                .get(device_name)
                .cloned()
                .unwrap_or_default();
            let endpoint_id = device_registry
                .id_for(backend, device_name)
                .unwrap_or_else(|| Self::fallback_endpoint_id(backend, device_name));
            device_registry.upsert_available(AudioEndpoint {
                id: endpoint_id,
                backend,
                device_name: device_name.clone(),
                channels,
                channel_layouts,
                availability: DeviceAvailability::Available,
            });
        }

        let input_devices = if include_full_device_metadata {
            Self::list_input_devices(&host)
        } else {
            Vec::new()
        };
        info!(
            "Enumerated {} input device(s) for {} (full metadata={}): {:?}",
            input_devices.len(),
            backend.label(),
            include_full_device_metadata,
            input_devices
        );

        let supported_sample_rates = if include_full_device_metadata {
            supported_output_sample_rates(&out_dev)
        } else {
            vec![output_config.sample_rate()]
        };
        let driver_buffer_sizes = if include_full_device_metadata {
            supported_output_buffer_sizes(&out_dev, backend)
        } else {
            Vec::new()
        };
        let supported_buffer_sizes = buffer_size_options_for_backend(backend, &driver_buffer_sizes);
        info!(
            "Selected output configuration for '{}': {} channels @ {} Hz, {:?}",
            out_name,
            preferred_output_channels,
            output_config.sample_rate(),
            output_config.sample_format()
        );
        let in_name = if include_full_device_metadata {
            host.default_input_device()
                .map(|device| device.to_string())
                .unwrap_or_else(|| "Default Input".to_string())
        } else {
            "Not enumerated for per-Cue endpoint worker".to_string()
        };

        info!(
            "Audio backend: {}; Output: {}, Input: {}",
            backend.label(),
            out_name,
            in_name
        );

        // Release the enumeration-thread CPAL endpoint/host before the render
        // thread resolves its own native handle. Some WASAPI vendor drivers
        // keep apartment-bound endpoint references alive for the host lifetime.
        drop(out_dev);
        drop(host);

        let (cmd_tx, cmd_rx) = bounded::<AudioCommand>(256);
        let (pos_tx, pos_rx) = bounded::<PositionUpdate>(1024);
        let output_settings_by_device = Arc::new(RwLock::new(HashMap::new()));
        let _initial_output_settings = shared_output_settings(
            &output_settings_by_device,
            &out_name,
            preferred_output_channels.max(1),
        );
        let worker_output_settings = output_settings_by_device.clone();
        let worker_default_device = out_name.clone();
        let worker_default_device_id = out_id.clone();
        let output_peak_meter = Arc::new(AtomicU32::new(0));
        let worker_output_peak_meter = output_peak_meter.clone();
        let (ready_tx, ready_rx) = bounded::<Result<(usize, u32), String>>(1);

        let audio_thread = std::thread::spawn(move || {
            audio_worker(
                backend,
                worker_default_device_id,
                worker_default_device,
                cmd_rx,
                pos_tx,
                worker_output_settings,
                worker_output_peak_meter,
                performance,
                ready_tx,
            );
        });

        let (output_channels, actual_sample_rate) = ready_rx
            .recv_timeout(Duration::from_secs(15))
            .map_err(|error| {
                anyhow::anyhow!("Audio output initialization did not respond: {error}")
            })?
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        info!(
            "Audio endpoint initialization completed in {:.1} ms: {} — '{}' ({} channels @ {} Hz; full metadata={})",
            endpoint_init_started.elapsed().as_secs_f64() * 1_000.0,
            backend.short_label(),
            out_name,
            output_channels,
            actual_sample_rate,
            include_full_device_metadata
        );
        let output_device_channels = {
            let mut channels = output_device_channels;
            channels.insert(out_name.clone(), output_channels);
            channels
        };
        let output_device_channel_layouts = {
            let mut layouts = output_device_channel_layouts;
            layouts.insert(out_name.clone(), vec![output_channels]);
            layouts
        };

        device_registry.upsert_available(AudioEndpoint {
            id: out_id.clone(),
            backend,
            device_name: out_name.clone(),
            channels: output_channels.max(1),
            channel_layouts: output_device_channel_layouts
                .get(&out_name)
                .cloned()
                .unwrap_or_else(|| vec![output_channels.max(1)]),
            availability: DeviceAvailability::Available,
        });

        let detected_default_output_pairs =
            Self::detect_default_pairs_for_devices(backend, &output_driver_channel_info);

        Ok(Self {
            backend,
            cmd_tx,
            pos_rx,
            output_devices,
            device_registry,
            input_devices,
            current_device: out_name,
            current_device_id: out_id,
            current_input_device: in_name,
            output_channels,
            current_sample_rate: actual_sample_rate,
            performance_settings: performance,
            supported_sample_rates,
            supported_buffer_sizes,
            output_device_channels,
            output_device_channel_layouts,
            output_settings_by_device,
            endpoint_output_channel_settings: Arc::new(RwLock::new(HashMap::new())),
            output_driver_channel_info,
            output_channel_names: HashMap::new(),
            default_output_pairs: HashMap::new(),
            output_channel_orders: HashMap::new(),
            detected_default_output_pairs,
            audio_thread: Some(audio_thread),
            worker_manager_thread: None,
            worker_manager_shutdown: None,
            worker_event_rx: None,
            output_peak_meter,
            worker_job_supervisor: None,
        })
    }

    #[cfg(target_os = "windows")]
    fn new_directsound(
        selected_device: Option<String>,
        selected_endpoint: Option<AudioEndpoint>,
        performance: AudioPerformanceSettings,
        include_full_device_metadata: bool,
    ) -> Result<Self> {
        let (cmd_tx, cmd_rx) = bounded::<AudioCommand>(256);
        let (pos_tx, pos_rx) = bounded::<PositionUpdate>(1024);
        let output_settings_by_device = Arc::new(RwLock::new(HashMap::new()));
        let worker_output_settings = output_settings_by_device.clone();
        let output_peak_meter = Arc::new(AtomicU32::new(0));
        let worker_output_peak_meter = output_peak_meter.clone();
        let (ready_tx, ready_rx) = bounded::<Result<DirectSoundInitInfo, String>>(1);

        let audio_thread = std::thread::spawn(move || {
            audio_worker_directsound(
                selected_device,
                selected_endpoint,
                include_full_device_metadata,
                cmd_rx,
                pos_tx,
                worker_output_settings,
                worker_output_peak_meter,
                performance,
                ready_tx,
            );
        });

        let ready = ready_rx
            .recv_timeout(Duration::from_secs(15))
            .map_err(|error| {
                anyhow::anyhow!("DirectSound initialization did not respond: {error}")
            })?
            .map_err(|error| anyhow::anyhow!("{error}"))?;

        let mut detected_default_output_pairs = HashMap::new();
        for (device_name, channels) in &ready.output_device_channels {
            if *channels >= 2 {
                detected_default_output_pairs.insert(device_name.clone(), [0, 1]);
            }
        }

        info!(
            "Audio backend: DirectSound; Output: {}; {} output device(s) detected",
            ready.current_device,
            ready.output_devices.len()
        );

        Ok(Self {
            backend: AudioBackend::DirectSound,
            cmd_tx,
            pos_rx,
            output_devices: ready.output_devices,
            device_registry: DeviceRegistry::default(),
            input_devices: vec!["Default Input".to_string()],
            current_device_id: Self::fallback_endpoint_id(
                AudioBackend::DirectSound,
                &ready.current_device,
            ),
            current_device: ready.current_device,
            current_input_device: "Default Input".to_string(),
            output_channels: ready.output_channels,
            current_sample_rate: ready.sample_rate,
            performance_settings: performance,
            supported_sample_rates: {
                let mut rates = COMMON_SAMPLE_RATES.to_vec();
                if !rates.contains(&ready.sample_rate) {
                    rates.push(ready.sample_rate);
                    rates.sort_unstable();
                }
                rates
            },
            supported_buffer_sizes: DIRECTSOUND_BUFFER_SIZES.to_vec(),
            output_device_channels: ready.output_device_channels,
            output_device_channel_layouts: ready.output_device_channel_layouts,
            output_settings_by_device,
            endpoint_output_channel_settings: Arc::new(RwLock::new(HashMap::new())),
            output_driver_channel_info: HashMap::new(),
            output_channel_names: HashMap::new(),
            default_output_pairs: HashMap::new(),
            output_channel_orders: HashMap::new(),
            detected_default_output_pairs,
            audio_thread: Some(audio_thread),
            worker_manager_thread: None,
            worker_manager_shutdown: None,
            worker_event_rx: None,
            output_peak_meter,
            worker_job_supervisor: None,
        })
    }

    fn normalize_device_name(value: &str) -> String {
        value
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .map(|character| character.to_ascii_lowercase())
            .collect()
    }

    fn detect_default_stereo_pair(channels: &[OutputChannelInfo]) -> Option<[usize; 2]> {
        let normalized = |name: &str| {
            name.chars()
                .filter(|character| character.is_ascii_alphanumeric())
                .map(|character| character.to_ascii_lowercase())
                .collect::<String>()
        };

        // Prefer explicit physical monitor/line-output labels. These are the
        // strongest signals that the driver knows the intended stereo pair.
        let left = channels.iter().find(|channel| {
            let name = normalized(&channel.name);
            name.contains("lineout1l")
                || name.contains("mainleft")
                || name.contains("mainl")
                || name.contains("monitorleft")
                || name.contains("monitorl")
                || name.contains("speakerleft")
                || name.contains("frontleft")
        });
        let right = channels.iter().find(|channel| {
            let name = normalized(&channel.name);
            name.contains("lineout2r")
                || name.contains("mainright")
                || name.contains("mainr")
                || name.contains("monitorright")
                || name.contains("monitorr")
                || name.contains("speakerright")
                || name.contains("frontright")
        });

        match (left, right) {
            (Some(left), Some(right)) if left.index != right.index => {
                Some([left.index, right.index])
            }
            _ => None,
        }
    }

    fn detect_default_pairs_for_devices(
        backend: AudioBackend,
        device_channels: &HashMap<String, Vec<OutputChannelInfo>>,
    ) -> HashMap<String, [usize; 2]> {
        if backend != AudioBackend::Asio {
            return HashMap::new();
        }

        device_channels
            .iter()
            .filter_map(|(device_name, channels)| {
                let pair = Self::detect_default_stereo_pair(channels)?;
                info!(
                    "Detected default stereo output pair for '{}': ASIO {} / ASIO {}",
                    device_name,
                    pair[0] + 1,
                    pair[1] + 1
                );
                Some((format!("{}::{}", backend.label(), device_name), pair))
            })
            .collect()
    }

    fn list_output_devices(host: &Host) -> Vec<String> {
        host.output_devices()
            .map(|devices| devices.map(|device| device.to_string()).collect())
            .unwrap_or_default()
    }

    /// Enumerate endpoint identities and supported channel layouts without
    /// opening streams. The registry monitor calls this on a background thread;
    /// capability queries provide the channel count needed by per-Cue routing.
    pub fn scan_output_endpoints() -> DeviceScanResult {
        Self::scan_output_endpoints_with_asio(true)
    }

    /// Lightweight hot-plug scan used while the application is running. ASIO
    /// driver inventory is intentionally not re-enumerated periodically because
    /// some vendor drivers are sensitive to repeated host initialization while
    /// an ASIO stream is active. The full ASIO list is loaded at application start.
    pub fn scan_hotplug_output_endpoints() -> DeviceScanResult {
        Self::scan_output_endpoints_with_asio(false)
    }

    fn scan_output_endpoints_with_asio(include_asio: bool) -> DeviceScanResult {
        let mut scan = DeviceScanResult::default();
        let backends = if include_asio {
            vec![AudioBackend::Wasapi, AudioBackend::Asio]
        } else {
            vec![AudioBackend::Wasapi]
        };

        for backend in backends {
            match host_for_backend(backend) {
                Ok(host) => match host.output_devices() {
                    Ok(devices) => {
                        scan.completed_backends.push(backend);
                        for device in devices {
                            let device_name = device.to_string();
                            let id = Self::cpal_endpoint_id(backend, &device, &device_name);
                            // Query supported layouts without opening a stream. Per-Cue
                            // routing needs this metadata before the endpoint worker starts.
                            let channel_layouts = supported_output_channel_layouts(&device);
                            let channels = channel_layouts.last().copied().unwrap_or(0);
                            info!(
                                "Scanned output endpoint '{}' [{}]: channels={}, layouts={:?}",
                                device_name, id, channels, channel_layouts
                            );
                            scan.endpoints.push(AudioEndpoint {
                                id,
                                backend,
                                device_name,
                                channels,
                                channel_layouts,
                                availability: DeviceAvailability::Available,
                            });
                        }
                    }
                    Err(error) => scan.errors.push(format!(
                        "{} output enumeration failed: {}",
                        backend.short_label(),
                        error
                    )),
                },
                Err(error) => scan.errors.push(format!(
                    "{} host is unavailable: {:#}",
                    backend.short_label(),
                    error
                )),
            }
        }

        #[cfg(target_os = "windows")]
        if scan.completed_backends.contains(&AudioBackend::Wasapi) {
            let wasapi_endpoints: Vec<AudioEndpoint> = scan
                .endpoints
                .iter()
                .filter(|endpoint| endpoint.backend == AudioBackend::Wasapi)
                .cloned()
                .collect();
            for endpoint in wasapi_endpoints {
                scan.endpoints.push(AudioEndpoint {
                    id: Self::fallback_endpoint_id(
                        AudioBackend::DirectSound,
                        &endpoint.device_name,
                    ),
                    backend: AudioBackend::DirectSound,
                    device_name: endpoint.device_name,
                    channels: endpoint.channels,
                    channel_layouts: endpoint.channel_layouts,
                    availability: DeviceAvailability::Available,
                });
            }
            scan.endpoints.push(AudioEndpoint {
                id: "directsound:default".to_string(),
                backend: AudioBackend::DirectSound,
                device_name: DIRECTSOUND_DEFAULT_DEVICE.to_string(),
                channels: 2,
                channel_layouts: vec![2],
                availability: DeviceAvailability::Available,
            });
            scan.completed_backends.push(AudioBackend::DirectSound);
        }

        scan.endpoints.sort_by(|left, right| {
            left.backend
                .short_label()
                .cmp(right.backend.short_label())
                .then_with(|| left.device_name.cmp(&right.device_name))
        });
        scan.endpoints.dedup_by(|left, right| left.id == right.id);

        info!(
            "Audio endpoint scan completed: {} endpoint(s); completed backends={:?}; errors={:?}",
            scan.endpoints.len(),
            scan.completed_backends
                .iter()
                .map(|backend| backend.short_label())
                .collect::<Vec<_>>(),
            scan.errors
        );
        scan
    }

    fn fallback_endpoint_id(backend: AudioBackend, device_name: &str) -> String {
        if backend == AudioBackend::DirectSound && device_name == DIRECTSOUND_DEFAULT_DEVICE {
            return "directsound:default".to_string();
        }
        format!(
            "{}:name:{}",
            backend.short_label().to_ascii_lowercase(),
            Self::normalize_device_name(device_name)
        )
    }

    fn cpal_endpoint_id(backend: AudioBackend, device: &Device, device_name: &str) -> String {
        match device.id() {
            Ok(id) => id.to_string(),
            Err(error) => {
                warn!(
                    "CPAL could not provide a stable ID for {} device '{}': {}; using a normalized-name fallback",
                    backend.short_label(),
                    device_name,
                    error
                );
                Self::fallback_endpoint_id(backend, device_name)
            }
        }
    }

    fn list_output_device_info(
        host: &Host,
    ) -> (HashMap<String, usize>, HashMap<String, Vec<usize>>) {
        let mut max_channels = HashMap::new();
        let mut layouts_by_device = HashMap::new();
        if let Ok(devices) = host.output_devices() {
            for device in devices {
                let name = device.to_string();
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
    /// after Theatre Show Cue started.
    pub fn refresh_output_device_info(&mut self, device_name: &str) {
        // SDL already supplied the DirectSound device channel counts during
        // backend initialization; CPAL cannot enumerate these endpoints.
        if self.backend == AudioBackend::DirectSound {
            return;
        }
        let Ok(host) = host_for_backend(self.backend) else {
            return;
        };
        let Ok(devices) = host.output_devices() else {
            return;
        };
        let Some(device) = devices
            .into_iter()
            .find(|device| device.to_string() == device_name)
        else {
            return;
        };
        let layouts = supported_output_channel_layouts(&device);
        self.supported_sample_rates = supported_output_sample_rates(&device);
        let driver_buffer_sizes = supported_output_buffer_sizes(&device, self.backend);
        self.supported_buffer_sizes =
            buffer_size_options_for_backend(self.backend, &driver_buffer_sizes);
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
        }
        let current = self.output_channel_settings_for_device(Some(device_name));
        self.set_output_channel_settings_for_device(device_name, current);
    }

    pub fn output_channel_layouts_for_device(&self, device_name: Option<&str>) -> Vec<usize> {
        let name = device_name.unwrap_or(self.current_device.as_str());
        self.output_device_channel_layouts
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    /// Physical output count for a selected cue endpoint. The stable registry
    /// identity is preferred so equal display names across backends stay distinct.
    pub fn output_channels_for_endpoint(
        &self,
        backend: AudioBackend,
        endpoint_id: Option<&str>,
        device_name: Option<&str>,
    ) -> usize {
        if backend == self.backend
            && endpoint_id.is_some_and(|id| id == self.current_device_id.as_str())
        {
            return self.output_channels.max(1);
        }

        if let Some(endpoint) = endpoint_id
            .and_then(|id| self.device_registry.find_by_id(id))
            .filter(|endpoint| endpoint.backend == backend)
        {
            return if endpoint.channels == 0 {
                2
            } else {
                endpoint.channels
            };
        }

        if let Some(name) = device_name {
            if backend == self.backend {
                if let Some(channels) = self.output_device_channels.get(name) {
                    return (*channels).max(1);
                }
            }
            if let Some(endpoint) = self.device_registry.find_by_backend_name(backend, name) {
                return if endpoint.channels == 0 {
                    2
                } else {
                    endpoint.channels
                };
            }
        }

        if backend == self.backend && device_name.is_none() {
            self.output_channels.max(1)
        } else {
            2
        }
    }

    /// Physical output count for a selected cue device on the active backend.
    /// `None` means the global default.
    pub fn output_channels_for_device(&self, device_name: Option<&str>) -> usize {
        let endpoint_id =
            device_name.and_then(|name| self.registered_endpoint_id(self.backend, name));
        let endpoint_id = endpoint_id.as_deref().or_else(|| {
            device_name
                .is_none()
                .then_some(self.current_device_id.as_str())
        });
        self.output_channels_for_endpoint(self.backend, endpoint_id, device_name)
    }

    fn output_device_config_key_for_backend(backend: AudioBackend, device_name: &str) -> String {
        format!("{}::{}", backend.label(), device_name)
    }

    fn output_device_config_key(&self, device_name: &str) -> String {
        Self::output_device_config_key_for_backend(self.backend, device_name)
    }

    /// Public key helper so AppSettings can persist calibration separately for
    /// the same hardware exposed through different backends.
    pub fn output_device_settings_key(&self, device_name: &str) -> String {
        self.output_device_config_key(device_name)
    }

    pub fn output_device_settings_key_for_backend(
        backend: AudioBackend,
        device_name: &str,
    ) -> String {
        Self::output_device_config_key_for_backend(backend, device_name)
    }

    /// Publish workstation-wide per-endpoint preferences to the worker manager.
    /// Project-scoped logical routing is intentionally not included here.
    pub fn set_endpoint_preferences(
        &self,
        performance_settings_by_device: HashMap<String, AudioPerformanceSettings>,
        output_channel_settings_by_device: HashMap<String, Vec<OutputChannelSetting>>,
    ) {
        *self.endpoint_output_channel_settings.write() = output_channel_settings_by_device.clone();
        self.send(AudioCommand::SetEndpointPreferences {
            performance_settings_by_device,
            output_channel_settings_by_device,
        });
    }

    /// Publish the latest endpoint availability snapshot to the worker manager.
    pub fn update_device_registry(&self) {
        self.send(AudioCommand::UpdateDeviceRegistry {
            endpoints: self.device_registry.snapshot_endpoints(),
        });
    }

    /// True when the isolated render thread exited while its Worker control
    /// process is still running. The child IPC loop uses this to surface native
    /// stream failures to the parent supervisor.
    pub(crate) fn audio_thread_finished(&self) -> bool {
        self.audio_thread
            .as_ref()
            .is_some_and(|audio_thread| audio_thread.is_finished())
    }

    pub(crate) fn drain_worker_manager_events(&self) -> Vec<AudioWorkerManagerEvent> {
        let mut events = Vec::new();
        if let Some(receiver) = self.worker_event_rx.as_ref() {
            while let Ok(event) = receiver.try_recv() {
                events.push(event);
            }
        }
        events
    }

    /// Take the maximum absolute sample from the final mixed output since the previous read.
    /// This is consumed by the audio worker's IPC loop, not by the output callback.
    pub(crate) fn take_output_peak(&self) -> f32 {
        f32::from_bits(self.output_peak_meter.swap(0, Ordering::Relaxed)).max(0.0)
    }

    /// Refresh global-output metadata after its endpoint worker has been
    /// recreated. Project routing maps are deliberately left untouched.
    pub(crate) fn apply_recovered_worker_metadata(&mut self, metadata: &WorkerEngineMetadata) {
        if metadata.backend != self.backend || metadata.current_device_id != self.current_device_id
        {
            return;
        }

        self.current_device = metadata.current_device.clone();
        self.output_devices = metadata.output_devices.clone();
        self.output_channels = metadata.output_channels.max(1);
        self.current_sample_rate = metadata.current_sample_rate;
        self.performance_settings = metadata.performance_settings;
        self.supported_sample_rates = metadata.supported_sample_rates.clone();
        self.supported_buffer_sizes = metadata.supported_buffer_sizes.clone();
        self.output_device_channels = metadata.output_device_channels.clone();
        self.output_device_channel_layouts = metadata.output_device_channel_layouts.clone();
        self.output_driver_channel_info = metadata.output_driver_channel_info.clone();
        self.detected_default_output_pairs = metadata.detected_default_output_pairs.clone();

        let _ = shared_output_settings(
            &self.output_settings_by_device,
            &self.current_device,
            self.output_channels,
        );
        info!(
            "Applied recovered global audio endpoint metadata: {} — '{}' ({} channels @ {} Hz)",
            self.backend.short_label(),
            self.current_device,
            self.output_channels,
            self.current_sample_rate
        );
    }

    fn default_output_channel_order_for(
        _backend: AudioBackend,
        _device_name: &str,
        channels: usize,
    ) -> Vec<usize> {
        (0..channels).collect()
    }

    /// Display logical Theatre Show Cue outputs in their configured order for
    /// a specific backend/endpoint. The map is project-scoped and keyed by
    /// backend + stable endpoint display name.
    pub fn output_channel_display_order_for_endpoint(
        &self,
        backend: AudioBackend,
        endpoint_id: Option<&str>,
        device_name: Option<&str>,
    ) -> Vec<usize> {
        let name = device_name.unwrap_or(self.current_device.as_str());
        let channels = self.output_channels_for_endpoint(backend, endpoint_id, device_name);
        let key = Self::output_device_config_key_for_backend(backend, name);
        let configured = self.output_channel_orders.get(&key);
        let mut order = Vec::with_capacity(channels);
        let mut used = std::collections::HashSet::new();

        if let Some(configured) = configured {
            for &index in configured {
                if index < channels && used.insert(index) {
                    order.push(index);
                }
            }
        } else {
            for index in Self::default_output_channel_order_for(backend, name, channels) {
                if used.insert(index) {
                    order.push(index);
                }
            }
        }

        for index in 0..channels {
            if used.insert(index) {
                order.push(index);
            }
        }
        order
    }

    pub fn output_channel_display_order(&self, device_name: Option<&str>) -> Vec<usize> {
        let endpoint_id =
            device_name.and_then(|name| self.registered_endpoint_id(self.backend, name));
        let endpoint_id = endpoint_id.as_deref().or_else(|| {
            (device_name.is_none() || device_name == Some(self.current_device.as_str()))
                .then_some(self.current_device_id.as_str())
        });
        self.output_channel_display_order_for_endpoint(self.backend, endpoint_id, device_name)
    }

    /// Replace the logical output order for one device. The order is normalized
    /// into a complete permutation before it is persisted.
    pub fn set_output_channel_order_for_device(&mut self, device_name: &str, order: Vec<usize>) {
        let channels = self.output_channels_for_device(Some(device_name));
        let mut normalized = Vec::with_capacity(channels);
        let mut used = std::collections::HashSet::new();
        for index in order {
            if index < channels && used.insert(index) {
                normalized.push(index);
            }
        }
        for index in 0..channels {
            if used.insert(index) {
                normalized.push(index);
            }
        }
        self.output_channel_orders
            .insert(self.output_device_config_key(device_name), normalized);
    }

    pub fn set_output_channel_orders(&mut self, orders: HashMap<String, Vec<usize>>) {
        self.output_channel_orders = orders;
    }

    /// Raw name reported by ASIOGetChannelInfo, when the driver supplies one.
    pub fn output_driver_channel_name(
        &self,
        device_name: Option<&str>,
        channel_index: usize,
    ) -> Option<String> {
        let name = device_name.unwrap_or(self.current_device.as_str());
        self.output_driver_channel_info
            .get(name)
            .and_then(|channels| {
                channels
                    .iter()
                    .find(|channel| channel.index == channel_index)
            })
            .map(|channel| channel.name.trim().to_string())
            .filter(|name| !name.is_empty())
    }

    /// Human-readable label for a native output channel, using the driver's
    /// reported name or an explicit user alias. No vendor-specific permutation
    /// or guessed Computer Bus labels are inserted here.
    pub fn output_channel_label(&self, device_name: Option<&str>, channel_index: usize) -> String {
        let name = device_name.unwrap_or(self.current_device.as_str());
        let key = self.output_device_config_key(name);
        let order = self.output_channel_display_order(Some(name));
        let logical_index = order
            .iter()
            .position(|&native_index| native_index == channel_index)
            .unwrap_or(channel_index);

        let alias = self
            .output_channel_names
            .get(&key)
            .and_then(|labels| labels.get(channel_index))
            .map(|label| label.trim())
            .filter(|label| !label.is_empty())
            .map(str::to_owned);

        let driver_name = self.output_driver_channel_name(Some(name), channel_index);
        let driver_name = driver_name.filter(|driver_name| {
            let normalized = Self::normalize_device_name(driver_name);
            let generic_output = normalized == format!("output{}", channel_index + 1)
                || normalized == format!("asiooutput{}", channel_index + 1)
                || normalized == format!("ch{}", channel_index + 1)
                || normalized == format!("channel{}", channel_index + 1);
            !generic_output
        });

        let base = alias.or(driver_name);

        match (self.backend, base) {
            (AudioBackend::Asio, Some(label)) => {
                format!("{} · ASIO {}", label, channel_index + 1)
            }
            (AudioBackend::Asio, None) => format!(
                "TaSQ Output {} · ASIO {}",
                logical_index + 1,
                channel_index + 1
            ),
            (_, Some(label)) => format!("{} · Ch. {}", label, channel_index + 1),
            (_, None) => format!(
                "TaSQ Output {} · Ch. {}",
                logical_index + 1,
                channel_index + 1
            ),
        }
    }

    pub fn output_channel_label_for_endpoint(
        &self,
        backend: AudioBackend,
        endpoint_id: Option<&str>,
        device_name: Option<&str>,
        channel_index: usize,
    ) -> String {
        let name = device_name.unwrap_or(self.current_device.as_str());
        let key = Self::output_device_config_key_for_backend(backend, name);
        let order =
            self.output_channel_display_order_for_endpoint(backend, endpoint_id, device_name);
        let logical_index = order
            .iter()
            .position(|&native_index| native_index == channel_index)
            .unwrap_or(channel_index);
        let alias = self
            .output_channel_names
            .get(&key)
            .and_then(|labels| labels.get(channel_index))
            .map(|label| label.trim())
            .filter(|label| !label.is_empty())
            .map(str::to_owned);
        let driver_name = if backend == self.backend {
            self.output_driver_channel_name(Some(name), channel_index)
        } else {
            None
        };
        let driver_name = driver_name.filter(|driver_name| {
            let normalized = Self::normalize_device_name(driver_name);
            let generic_output = normalized == format!("output{}", channel_index + 1)
                || normalized == format!("asiooutput{}", channel_index + 1)
                || normalized == format!("ch{}", channel_index + 1)
                || normalized == format!("channel{}", channel_index + 1);
            !generic_output
        });
        let base = alias.or(driver_name);

        match (backend, base) {
            (AudioBackend::Asio, Some(label)) => {
                format!("{} · ASIO {}", label, channel_index + 1)
            }
            (AudioBackend::Asio, None) => format!(
                "TaSQ Output {} · ASIO {}",
                logical_index + 1,
                channel_index + 1
            ),
            (_, Some(label)) => format!("{} · Ch. {}", label, channel_index + 1),
            (_, None) => format!(
                "TaSQ Output {} · Ch. {}",
                logical_index + 1,
                channel_index + 1
            ),
        }
    }

    pub fn detected_default_output_pair_for_device(
        &self,
        device_name: Option<&str>,
    ) -> Option<[usize; 2]> {
        let name = device_name.unwrap_or(self.current_device.as_str());
        self.detected_default_output_pairs
            .get(&self.output_device_config_key(name))
            .copied()
    }

    pub fn output_channel_settings_for_endpoint(
        &self,
        backend: AudioBackend,
        endpoint_id: Option<&str>,
        device_name: Option<&str>,
    ) -> Vec<OutputChannelSetting> {
        let channels = self.output_channels_for_endpoint(backend, endpoint_id, device_name);
        let resolved_name = device_name
            .map(str::to_owned)
            .or_else(|| {
                endpoint_id
                    .and_then(|id| self.device_registry.find_by_id(id))
                    .filter(|endpoint| endpoint.backend == backend)
                    .map(|endpoint| endpoint.device_name.clone())
            })
            .or_else(|| (backend == self.backend).then(|| self.current_device.clone()));
        let Some(resolved_name) = resolved_name else {
            return vec![OutputChannelSetting::default(); channels];
        };
        let key = Self::output_device_config_key_for_backend(backend, &resolved_name);

        let saved_settings = self
            .endpoint_output_channel_settings
            .read()
            .get(&key)
            .cloned();
        if let Some(mut settings) = saved_settings {
            settings.resize(channels, OutputChannelSetting::default());
            settings.truncate(channels);
            return settings;
        }

        if backend == self.backend {
            return self.output_channel_settings_for_device(Some(&resolved_name));
        }

        vec![OutputChannelSetting::default(); channels]
    }

    pub fn output_channel_names_for_device(&self, device_name: Option<&str>) -> Vec<String> {
        let name = device_name.unwrap_or(self.current_device.as_str());
        let key = self.output_device_config_key(name);
        self.output_channel_names
            .get(&key)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_output_channel_names(&mut self, names: HashMap<String, Vec<String>>) {
        self.output_channel_names = names;
    }

    pub fn set_output_channel_names_for_device(&mut self, device_name: &str, names: Vec<String>) {
        let key = self.output_device_config_key(device_name);
        self.output_channel_names.insert(key, names);
    }

    /// The default stereo pair is a pair of stable logical TaSQ output indices.
    /// Changing the native mapping must not alter it or existing Cue routes.
    fn resolve_default_output_pair(
        explicit_pair: Option<[usize; 2]>,
        channel_count: usize,
    ) -> [usize; 2] {
        explicit_pair
            .filter(|pair| pair[0] < channel_count && pair[1] < channel_count && pair[0] != pair[1])
            .unwrap_or([0, if channel_count > 1 { 1 } else { 0 }])
    }

    pub fn native_output_channel_for_logical(
        order: &[usize],
        logical_channel: usize,
    ) -> Option<usize> {
        order.get(logical_channel).copied()
    }

    pub fn default_output_pair_for_device(&self, device_name: Option<&str>) -> [usize; 2] {
        let name = device_name.unwrap_or(self.current_device.as_str());
        let key = self.output_device_config_key(name);
        let channels = self.output_channels_for_device(Some(name));
        Self::resolve_default_output_pair(self.default_output_pairs.get(&key).copied(), channels)
    }

    /// Convert Cue-owned logical destinations to the stream channel positions.
    /// The Cue's saved routes are NEVER rewritten by this operation.
    pub fn map_cue_routes_to_native(
        logical_routes: &[ChannelRoute],
        order: &[usize],
    ) -> Vec<ChannelRoute> {
        logical_routes
            .iter()
            .map(|route| {
                let mut result = route.clone();
                result.output_channel =
                    Self::native_output_channel_for_logical(order, route.output_channel)
                        .unwrap_or(route.output_channel);
                result.output_channels = route
                    .output_channels
                    .iter()
                    .filter_map(|logical_index| {
                        Self::native_output_channel_for_logical(order, *logical_index)
                    })
                    .collect();
                result.output_channels.sort_unstable();
                result.output_channels.dedup();
                result
            })
            .collect()
    }

    pub fn map_cue_gains_to_native(logical_gains: &[f32], order: &[usize]) -> Vec<f32> {
        let mut native_gains = vec![0.0; order.len()];
        for (logical_index, &native_index) in order.iter().enumerate() {
            if let (Some(&gain), Some(native_gain)) = (
                logical_gains.get(logical_index),
                native_gains.get_mut(native_index),
            ) {
                *native_gain = gain;
            }
        }
        native_gains
    }

    pub fn cue_routes_for_endpoint(
        &self,
        backend: AudioBackend,
        endpoint_id: Option<&str>,
        device_name: Option<&str>,
        logical_routes: &[ChannelRoute],
    ) -> Vec<ChannelRoute> {
        let order =
            self.output_channel_display_order_for_endpoint(backend, endpoint_id, device_name);
        Self::map_cue_routes_to_native(logical_routes, &order)
    }

    pub fn cue_gains_for_endpoint(
        &self,
        backend: AudioBackend,
        endpoint_id: Option<&str>,
        device_name: Option<&str>,
        logical_gains: &[f32],
    ) -> Vec<f32> {
        let order =
            self.output_channel_display_order_for_endpoint(backend, endpoint_id, device_name);
        Self::map_cue_gains_to_native(logical_gains, &order)
    }

    pub fn default_output_pair_for_endpoint(
        &self,
        backend: AudioBackend,
        endpoint_id: Option<&str>,
        device_name: Option<&str>,
    ) -> [usize; 2] {
        let name = device_name.unwrap_or(self.current_device.as_str());
        let key = Self::output_device_config_key_for_backend(backend, name);
        let channels = self.output_channels_for_endpoint(backend, endpoint_id, device_name);
        Self::resolve_default_output_pair(self.default_output_pairs.get(&key).copied(), channels)
    }

    pub fn default_route_channels_for_source_endpoint(
        &self,
        backend: AudioBackend,
        endpoint_id: Option<&str>,
        device_name: Option<&str>,
        source_channels: usize,
    ) -> Vec<usize> {
        let channels = self.output_channels_for_endpoint(backend, endpoint_id, device_name);
        if source_channels <= 2 {
            let pair = self.default_output_pair_for_endpoint(backend, endpoint_id, device_name);
            if pair[0] < channels && pair[1] < channels {
                vec![pair[0], pair[1]]
            } else {
                Vec::new()
            }
        } else {
            (0..channels.min(source_channels)).collect()
        }
    }

    pub fn set_default_output_pairs(&mut self, pairs: HashMap<String, [usize; 2]>) {
        self.default_output_pairs = pairs;
    }

    pub fn set_default_output_pair_for_device(&mut self, device_name: &str, pair: [usize; 2]) {
        let key = self.output_device_config_key(device_name);
        self.default_output_pairs.insert(key, pair);
    }

    /// Default logical TaSQ output indices for the first two source channels. A configured
    /// pair is used for new cues; existing per-cue routes remain untouched.
    pub fn default_route_output_channels(&self, device_name: Option<&str>) -> Vec<usize> {
        let pair = self.default_output_pair_for_device(device_name);
        let channels = self.output_channels_for_device(device_name);
        if pair[0] < channels && pair[1] < channels {
            vec![pair[0], pair[1]]
        } else {
            Vec::new()
        }
    }

    /// Default destinations use logical TaSQ output slots only. Changing the
    /// hardware mapping does not rewrite any Cue's I/O Routing assignments.
    pub fn default_route_channels_for_source(
        &self,
        device_name: Option<&str>,
        source_channels: usize,
    ) -> Vec<usize> {
        if source_channels <= 2 {
            self.default_route_output_channels(device_name)
        } else {
            (0..self
                .output_channels_for_device(device_name)
                .min(source_channels))
                .collect()
        }
    }

    pub fn test_output_channel(&self, device_name: &str, logical_channel: usize) {
        let output_order = self.output_channel_display_order(Some(device_name));
        let Some(native_channel) =
            Self::native_output_channel_for_logical(&output_order, logical_channel)
        else {
            warn!(
                "Cannot test logical output {} for '{}': output mapping has only {} entries",
                logical_channel + 1,
                device_name,
                output_order.len()
            );
            return;
        };
        let endpoint_id = if device_name == self.current_device.as_str() {
            self.current_device_id.as_str()
        } else {
            self.device_registry
                .find_available_by_backend_name(self.backend, device_name)
                .map(|endpoint| endpoint.id.as_str())
                .unwrap_or("unregistered")
        };
        info!(
            "Output test requested: backend='{}', endpoint_id='{}', device='{}', logical_output={}, mapped_native_channel={}",
            self.backend.short_label(),
            endpoint_id,
            device_name,
            logical_channel + 1,
            native_channel + 1
        );
        self.send(AudioCommand::TestOutput {
            device_name: device_name.to_string(),
            logical_channel,
            channel: native_channel,
        });
    }

    fn list_input_devices(host: &Host) -> Vec<String> {
        host.input_devices()
            .map(|devices| devices.map(|device| device.to_string()).collect())
            .unwrap_or_default()
    }

    /// Resolve a saved stable endpoint ID to its current display/native name.
    pub fn resolve_registered_endpoint_name(&self, id: &str) -> Option<String> {
        self.device_registry
            .find_available_by_id(id)
            .map(|endpoint| endpoint.device_name.clone())
    }

    pub fn registered_endpoint_id(
        &self,
        backend: AudioBackend,
        device_name: &str,
    ) -> Option<String> {
        self.device_registry.id_for(backend, device_name)
    }

    pub fn registered_endpoint_available(
        &self,
        backend: AudioBackend,
        id: Option<&str>,
        device_name: &str,
    ) -> bool {
        if let Some(id) = id {
            return self
                .device_registry
                .find_available_by_id(id)
                .is_some_and(|endpoint| endpoint.backend == backend);
        }
        self.device_registry
            .find_available_by_backend_name(backend, device_name)
            .is_some()
    }

    /// Resolve a UI/project endpoint name to the exact active-backend device name.
    /// DirectSound names are normalized because SDL and WASAPI may differ in punctuation.
    pub fn resolve_output_device_name(&self, name: &str) -> Option<String> {
        self.output_devices
            .iter()
            .find(|device| {
                device.as_str() == name
                    || (self.backend == AudioBackend::DirectSound
                        && Self::normalize_device_name(device.as_str())
                            == Self::normalize_device_name(name))
            })
            .cloned()
    }

    /// Rebuild the active global endpoint stream inside its owning worker.
    /// Other endpoint workers are unaffected.
    pub fn reconfigure_output(&mut self, performance: AudioPerformanceSettings) -> Result<()> {
        let performance = AudioPerformanceSettings::for_backend(self.backend, performance);
        let (reply_tx, reply_rx) = bounded::<Result<(usize, u32), String>>(1);
        self.cmd_tx
            .send(AudioCommand::ReconfigureOutput {
                performance,
                reply: reply_tx,
            })
            .map_err(|error| {
                anyhow::anyhow!("Could not send audio performance reconfiguration: {error}")
            })?;

        let result = reply_rx
            .recv_timeout(Duration::from_secs(45))
            .map_err(|error| {
                anyhow::anyhow!(
                    "Audio worker manager did not confirm performance reconfiguration: {error}"
                )
            })?
            .map_err(|error| anyhow::anyhow!("{error}"))?;

        self.output_channels = result.0.max(1);
        self.current_sample_rate = result.1;
        self.performance_settings = performance;
        self.output_device_channels
            .insert(self.current_device.clone(), self.output_channels);
        Ok(())
    }

    /// Switch the global-output pointer to an endpoint worker. If this endpoint
    /// has not been opened yet, the manager starts an isolated child process;
    /// workers already serving other Cues remain alive.
    pub fn set_output_endpoint(
        &mut self,
        endpoint_id: &str,
        name: &str,
        performance: AudioPerformanceSettings,
    ) -> Result<()> {
        let performance = AudioPerformanceSettings::for_backend(self.backend, performance);
        let resolved_name = self
            .device_registry
            .find_available_by_id(endpoint_id)
            .filter(|endpoint| endpoint.backend == self.backend)
            .map(|endpoint| endpoint.device_name.clone())
            .or_else(|| self.resolve_output_device_name(name))
            .unwrap_or_else(|| name.to_string());

        if self.current_device == resolved_name && self.current_device_id == endpoint_id {
            return Ok(());
        }

        self.refresh_output_device_info(&resolved_name);
        let (reply_tx, reply_rx) = bounded::<Result<(usize, u32), String>>(1);
        self.cmd_tx
            .send(AudioCommand::SetOutputDevice {
                device_name: resolved_name.clone(),
                endpoint_id: Some(endpoint_id.to_string()),
                performance,
                reply: reply_tx,
            })
            .map_err(|error| {
                anyhow::anyhow!("Could not send output-device switch command: {error}")
            })?;

        let switch_result = reply_rx
            .recv_timeout(Duration::from_secs(45))
            .map_err(|error| {
                anyhow::anyhow!(
                    "Audio worker manager did not confirm output-device switch: {error}"
                )
            })
            .and_then(|result| result.map_err(|error| anyhow::anyhow!("{error}")));
        let (channels, _sample_rate) = match switch_result {
            Ok(result) => result,
            Err(error) => {
                let previous_device = self.current_device.clone();
                self.refresh_output_device_info(&previous_device);
                return Err(error);
            }
        };

        self.current_device = resolved_name.clone();
        self.current_device_id = endpoint_id.to_string();
        self.output_channels = channels.max(1);
        self.current_sample_rate = _sample_rate;
        self.performance_settings = performance;
        self.output_device_channels
            .insert(resolved_name.clone(), self.output_channels);
        let layouts = self
            .output_device_channel_layouts
            .entry(resolved_name.clone())
            .or_default();
        if !layouts.contains(&self.output_channels) {
            layouts.push(self.output_channels);
            layouts.sort_unstable();
        }
        let current = self.output_channel_settings_for_device(Some(&resolved_name));
        self.set_output_channel_settings_for_device(&resolved_name, current);
        self.device_registry.upsert_available(AudioEndpoint {
            id: endpoint_id.to_string(),
            backend: self.backend,
            device_name: resolved_name.clone(),
            channels: self.output_channels,
            channel_layouts: self
                .output_device_channel_layouts
                .get(&resolved_name)
                .cloned()
                .unwrap_or_default(),
            availability: DeviceAvailability::Available,
        });
        info!(
            "AudioEngine accepted output endpoint '{} — {}' [{}] with {} channels",
            self.backend.short_label(),
            resolved_name,
            endpoint_id,
            self.output_channels
        );
        Ok(())
    }

    pub fn output_channel_settings(&self) -> Vec<OutputChannelSetting> {
        self.output_channel_settings_for_device(Some(&self.current_device))
    }

    pub fn set_output_channel_settings(&self, settings: Vec<OutputChannelSetting>) {
        self.set_output_channel_settings_for_device(&self.current_device, settings);
    }

    pub fn output_channel_settings_for_device(
        &self,
        device_name: Option<&str>,
    ) -> Vec<OutputChannelSetting> {
        let name = device_name.unwrap_or(self.current_device.as_str());
        let channels = self.output_channels_for_device(Some(name));
        let shared = shared_output_settings(&self.output_settings_by_device, name, channels);
        // Drop the read guard before `shared` so its lifetime does not extend
        // to the end of this block (required by the borrow checker).
        let settings = shared.read().clone();
        settings
    }

    pub fn set_output_channel_settings_for_device(
        &self,
        device_name: &str,
        settings: Vec<OutputChannelSetting>,
    ) {
        let channels = self.output_channels_for_device(Some(device_name));
        let shared = shared_output_settings(&self.output_settings_by_device, device_name, channels);
        {
            let mut current = shared.write();
            *current = settings.clone();
            current.resize(channels, OutputChannelSetting::default());
            current.truncate(channels);
        }
        let settings_key = Self::output_device_config_key_for_backend(self.backend, device_name);
        self.endpoint_output_channel_settings
            .write()
            .insert(settings_key, settings.clone());

        if self.worker_manager_thread.is_some() {
            self.send(AudioCommand::SetOutputChannelSettings {
                device_name: device_name.to_string(),
                settings,
            });
        }
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

    pub(crate) fn shutdown_and_wait(&mut self) {
        info!("AudioEngine shutdown requested");

        // The GUI-side manager owns every child process. Worker-child mode has
        // no manager and only joins its local render thread.
        if let Some(shutdown_requested) = self.worker_manager_shutdown.take() {
            shutdown_requested.store(true, Ordering::Release);
        }
        let _ = self
            .cmd_tx
            .send_timeout(AudioCommand::Shutdown, Duration::from_millis(250));

        if let Some(manager_thread) = self.worker_manager_thread.take() {
            info!(
                "Waiting up to {:?} for the audio worker manager to shut down",
                AUDIO_MANAGER_SHUTDOWN_TIMEOUT
            );
            let (joined_tx, joined_rx) = bounded::<bool>(1);
            let joiner = std::thread::Builder::new()
                .name("tasq-audio-manager-joiner".to_string())
                .spawn(move || {
                    let joined_cleanly = manager_thread.join().is_ok();
                    let _ = joined_tx.send(joined_cleanly);
                });

            match joiner {
                Ok(_joiner) => {
                    match joined_rx.recv_timeout(AUDIO_MANAGER_SHUTDOWN_TIMEOUT) {
                        Ok(true) => info!("Audio worker manager joined cleanly"),
                        Ok(false) => error!("Audio worker manager panicked during shutdown"),
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                            error!(
                            "Audio worker manager did not stop within {:?}; force-terminating its isolated worker processes",
                            AUDIO_MANAGER_SHUTDOWN_TIMEOUT
                        );
                            if let Some(supervisor) = self.worker_job_supervisor.as_ref() {
                                supervisor.terminate_all();
                            }
                            match joined_rx.recv_timeout(AUDIO_MANAGER_FORCE_SHUTDOWN_GRACE) {
                            Ok(true) => info!(
                                "Audio worker manager stopped after forced worker-process termination"
                            ),
                            Ok(false) => error!(
                                "Audio worker manager panicked after forced worker-process termination"
                            ),
                            Err(_) => error!(
                                "Audio worker manager is still blocked after forced worker-process termination; detaching its joiner so application shutdown can continue"
                            ),
                        }
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                            error!("Audio worker manager join notification channel closed unexpectedly");
                            if let Some(supervisor) = self.worker_job_supervisor.as_ref() {
                                supervisor.terminate_all();
                            }
                        }
                    }
                }
                Err(error) => {
                    error!(
                        "Could not start bounded audio-manager joiner: {}; force-terminating isolated worker processes",
                        error
                    );
                    if let Some(supervisor) = self.worker_job_supervisor.as_ref() {
                        supervisor.terminate_all();
                    }
                }
            }
        }

        if let Some(audio_thread) = self.audio_thread.take() {
            info!("Waiting for the in-process audio render thread to exit");
            if audio_thread.join().is_err() {
                error!("In-process audio render thread panicked during shutdown");
            }
        }

        info!("AudioEngine shutdown sequence completed");
    }

    pub(crate) fn worker_metadata(&self) -> crate::audio::worker::WorkerEngineMetadata {
        crate::audio::worker::WorkerEngineMetadata {
            backend: self.backend,
            output_devices: self.output_devices.clone(),
            registry_endpoints: self.device_registry.snapshot_endpoints(),
            input_devices: self.input_devices.clone(),
            current_device: self.current_device.clone(),
            current_device_id: self.current_device_id.clone(),
            current_input_device: self.current_input_device.clone(),
            output_channels: self.output_channels,
            current_sample_rate: self.current_sample_rate,
            performance_settings: self.performance_settings,
            supported_sample_rates: self.supported_sample_rates.clone(),
            supported_buffer_sizes: self.supported_buffer_sizes.clone(),
            output_device_channels: self.output_device_channels.clone(),
            output_device_channel_layouts: self.output_device_channel_layouts.clone(),
            output_driver_channel_info: self.output_driver_channel_info.clone(),
            detected_default_output_pairs: self.detected_default_output_pairs.clone(),
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.shutdown_and_wait();
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

const STOP_DECLICK_FADE_MS: f64 = 5.0;

/// Shared control between the audio worker and one Cue's render source.
/// The stop ramp is applied sample-by-sample after channel mapping/resampling.
#[derive(Clone)]
struct StopFadeControl {
    requested: Arc<AtomicBool>,
    completed: Arc<AtomicBool>,
}

impl StopFadeControl {
    fn new() -> Self {
        Self {
            requested: Arc::new(AtomicBool::new(false)),
            completed: Arc::new(AtomicBool::new(false)),
        }
    }

    fn request(&self) {
        if !self.completed.load(Ordering::Acquire) {
            self.requested.store(true, Ordering::Release);
        }
    }

    fn complete_without_render(&self) {
        self.completed.store(true, Ordering::Release);
    }

    fn is_completed(&self) -> bool {
        self.completed.load(Ordering::Acquire)
    }
}

struct PendingStopCompletion {
    request_id: Uuid,
    cue_id: Option<Uuid>,
    requested_at: Instant,
}

struct ActivePlayer {
    id: Uuid,
    generation: u64,
    file: PathBuf,
    sink: rodio::Player,
    start_secs: f64,
    end_secs: f64,
    base_volume: f32,
    current_volume: f32,
    volume_automation_enabled: bool,
    volume_automation: Vec<[f64; 2]>,
    time_pitch: Arc<RwLock<TimePitchSettings>>,
    position_state: Arc<AtomicU64>,
    prefetch_underrun_count: Arc<AtomicU64>,
    last_prefetch_underrun_count: u64,
    prefetch_underrun_frame_count: Arc<AtomicU64>,
    last_prefetch_underrun_frame_count: u64,
    automation_update_tx: Sender<VolumeAutomationUpdate>,
    automation_update_rx: Receiver<VolumeAutomationUpdate>,
    fade_in_secs: f64,
    fade_out_secs: f64,
    fade_curve: FadeCurve,
    custom_fade_points: Vec<[f32; 2]>,
    channel_routes: Arc<RwLock<Vec<ChannelRoute>>>,
    output_gains: Arc<RwLock<Vec<f32>>>,
    output_channels: usize,
    output_sample_rate: u32,
    output_device: String,
    uses_global_default: bool,
    mixer: rodio::mixer::Mixer,
    output_settings: SharedOutputSettings,
    loop_enabled: bool,
    loop_runtime_active: bool,
    loop_start: f64,
    loop_end: f64,
    loops_remaining: Arc<AtomicI32>, // -1 = infinite; finite count includes initial pass
    /// Producer-side count: decoder has prepared a loop repeat, not yet rendered it.
    loop_iterations: Arc<AtomicU64>,
    last_reported_loop_iterations: u64,
    /// Render-side count: the PCM consumer has crossed an audible loop boundary.
    render_loop_iterations: Arc<AtomicU64>,
    last_reported_render_loop_iterations: u64,
    render_loop_boundary_frame: Arc<AtomicU64>,
    render_loop_boundary_position_bits: Arc<AtomicU64>,
    render_loop_boundary_endpoint_frame: Arc<AtomicU64>,
    render_loop_fingerprint: Arc<AtomicU64>,
    render_loop_fingerprint_iteration: Arc<AtomicU64>,
    last_reported_render_loop_fingerprint_iteration: u64,
    loop_pcm_capture: Arc<LoopPcmCapture>,
    last_reported_loop_pcm_capture_iteration: u64,
    playback_elapsed_frames: Arc<AtomicU64>,
    render_loop_remaining_state: Arc<AtomicI32>,
    source_sample_rate: u32,
    paused_at: Option<f64>,
    dynamic_fade: Option<DynamicFade>,
    stop_fade: StopFadeControl,
    stopping: bool,
    pending_stop_completions: Vec<PendingStopCompletion>,
    /// Set by the render source on its first scheduled output frame for a batch Cue.
    start_frame_observed: Option<Arc<AtomicU64>>,
    start_frame_reported: bool,
    launch_transaction_id: Option<Uuid>,
}

impl ActivePlayer {
    fn current_pos(&self) -> f64 {
        self.paused_at
            .unwrap_or_else(|| atomic_load_f64(&self.position_state))
    }
}

fn db_to_linear(db: f32) -> f32 {
    if db <= -60.0 {
        0.0
    } else {
        10.0_f32.powf(db / 20.0)
    }
}

#[derive(Debug, Clone)]
struct VolumeAutomationUpdate {
    enabled: bool,
    points: Vec<[f64; 2]>,
}

/// Evaluate a piecewise-linear volume envelope. Points are [source_time_secs, gain].
fn volume_automation_gain_at(position_secs: f64, points: &[[f64; 2]], enabled: bool) -> f32 {
    if !enabled || points.is_empty() {
        return 1.0;
    }

    let first = points[0];
    if position_secs <= first[0] {
        return first[1].clamp(0.0, 1.0) as f32;
    }

    for pair in points.windows(2) {
        let left = pair[0];
        let right = pair[1];
        if position_secs <= right[0] {
            let span = right[0] - left[0];
            if span <= f64::EPSILON {
                return right[1].clamp(0.0, 1.0) as f32;
            }
            let t = ((position_secs - left[0]) / span).clamp(0.0, 1.0);
            let gain = left[1] + (right[1] - left[1]) * t;
            return gain.clamp(0.0, 1.0) as f32;
        }
    }

    points
        .last()
        .map(|point| point[1].clamp(0.0, 1.0) as f32)
        .unwrap_or(1.0)
}

/// Applies volume automation per audio frame (not on the 5 ms UI/control tick),
/// so ramps remain smooth and stereo channels always receive the same gain.
/// Live point edits arrive through a non-blocking channel and are adopted at
/// the next frame boundary without restarting playback.
struct VolumeAutomationSource<S> {
    source: S,
    channels: u64,
    sample_index: u64,
    position_state: Arc<AtomicU64>,
    current_gain: f32,
    enabled: bool,
    points: Vec<[f64; 2]>,
    updates: Receiver<VolumeAutomationUpdate>,
    retired_points_tx: Sender<Vec<[f64; 2]>>,
}

impl<S> VolumeAutomationSource<S>
where
    S: Source,
{
    fn new(
        source: S,
        enabled: bool,
        points: Vec<[f64; 2]>,
        updates: Receiver<VolumeAutomationUpdate>,
        retired_points_tx: Sender<Vec<[f64; 2]>>,
        position_state: Arc<AtomicU64>,
    ) -> Self {
        let channels = source.channels().get().max(1) as u64;
        Self {
            source,
            channels,
            sample_index: 0,
            position_state,
            current_gain: 1.0,
            enabled,
            points,
            updates,
            retired_points_tx,
        }
    }
}

fn retire_automation_points(retired_points_tx: &Sender<Vec<[f64; 2]>>, points: Vec<[f64; 2]>) {
    if let Err(error) = retired_points_tx.try_send(points) {
        // The control loop drains this bounded queue every worker tick. On an
        // exceptional overflow/disconnect, leak rather than free on callback.
        std::mem::forget(error.into_inner());
    }
}

impl<S> Iterator for VolumeAutomationSource<S>
where
    S: Source,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        let sample = self.source.next()?;
        let channel_index = self.sample_index % self.channels;
        if channel_index == 0 {
            while let Ok(update) = self.updates.try_recv() {
                self.enabled = update.enabled;
                let previous_points = std::mem::replace(&mut self.points, update.points);
                // The audio render thread must not free an automation Vec. The
                // Worker control loop owns the receiver and drops retired arrays.
                retire_automation_points(&self.retired_points_tx, previous_points);
            }
            let position_secs = atomic_load_f64(&self.position_state);
            self.current_gain =
                volume_automation_gain_at(position_secs, &self.points, self.enabled);
        }
        self.sample_index = self.sample_index.wrapping_add(1);
        Some(sample * self.current_gain)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.source.size_hint()
    }
}

impl<S> Drop for VolumeAutomationSource<S> {
    fn drop(&mut self) {
        let points = std::mem::take(&mut self.points);
        retire_automation_points(&self.retired_points_tx, points);
        while let Ok(update) = self.updates.try_recv() {
            retire_automation_points(&self.retired_points_tx, update.points);
        }
    }
}

impl<S> Source for VolumeAutomationSource<S>
where
    S: Source,
{
    fn current_span_len(&self) -> Option<usize> {
        // Keep the source format stable across spans; this also avoids splitting
        // multichannel frames at arbitrary sample boundaries in Rodio.
        None
    }

    fn channels(&self) -> std::num::NonZeroU16 {
        self.source.channels()
    }

    fn sample_rate(&self) -> std::num::NonZeroU32 {
        self.source.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.source.total_duration()
    }
}

/// Measures the final summed output signal without locking or changing PCM.
/// It publishes one atomic sample peak per 1,024 samples; the worker reads the
/// accumulator outside the audio callback. This runs once per output stream,
/// after all Cue sources for that endpoint have been mixed.
struct OutputPeakMeterSource<S> {
    source: S,
    peak: Arc<AtomicU32>,
    local_peak: f32,
    samples_since_publish: usize,
}

impl<S> OutputPeakMeterSource<S> {
    fn new(source: S, peak: Arc<AtomicU32>) -> Self {
        Self {
            source,
            peak,
            local_peak: 0.0,
            samples_since_publish: 0,
        }
    }

    fn publish_peak(&mut self) {
        if self.local_peak > 0.0 && self.local_peak.is_finite() {
            self.peak
                .fetch_max(self.local_peak.to_bits(), Ordering::Relaxed);
            self.local_peak = 0.0;
        }
        self.samples_since_publish = 0;
    }
}

impl<S> Iterator for OutputPeakMeterSource<S>
where
    S: Source,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        let Some(sample) = self.source.next() else {
            self.publish_peak();
            return None;
        };
        let magnitude = sample.abs();
        if magnitude.is_finite() {
            self.local_peak = self.local_peak.max(magnitude);
        }
        self.samples_since_publish += 1;
        if self.samples_since_publish >= 1_024 {
            self.publish_peak();
        }
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.source.size_hint()
    }
}

impl<S> Drop for OutputPeakMeterSource<S> {
    fn drop(&mut self) {
        self.publish_peak();
    }
}

impl<S> Source for OutputPeakMeterSource<S>
where
    S: Source,
{
    fn current_span_len(&self) -> Option<usize> {
        self.source.current_span_len()
    }

    fn channels(&self) -> std::num::NonZeroU16 {
        self.source.channels()
    }

    fn sample_rate(&self) -> std::num::NonZeroU32 {
        self.source.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.source.total_duration()
    }

    fn try_seek(
        &mut self,
        position: Duration,
    ) -> std::result::Result<(), rodio::source::SeekError> {
        self.source.try_seek(position)
    }
}

/// Counts complete interleaved frames delivered to the endpoint. The transport
/// gate is upstream of this wrapper, so silent Pause All frames still advance
/// the endpoint clock while the underlying Cue mixer remains frozen.
struct SampleFrameClockSource<S> {
    source: S,
    frame_clock: Arc<AtomicU64>,
    channels: usize,
    channel_index: usize,
}

impl<S> SampleFrameClockSource<S> {
    fn new(source: S, frame_clock: Arc<AtomicU64>, channels: usize) -> Self {
        Self {
            source,
            frame_clock,
            channels: channels.max(1),
            channel_index: 0,
        }
    }
}

impl<S> Iterator for SampleFrameClockSource<S>
where
    S: Source,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        let sample = self.source.next()?;
        self.channel_index += 1;
        if self.channel_index >= self.channels {
            self.channel_index = 0;
            self.frame_clock.fetch_add(1, Ordering::Relaxed);
        }
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.source.size_hint()
    }
}

impl<S> Source for SampleFrameClockSource<S>
where
    S: Source,
{
    fn current_span_len(&self) -> Option<usize> {
        self.source.current_span_len()
    }

    fn channels(&self) -> std::num::NonZeroU16 {
        self.source.channels()
    }

    fn sample_rate(&self) -> std::num::NonZeroU32 {
        self.source.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.source.total_duration()
    }

    fn try_seek(
        &mut self,
        position: Duration,
    ) -> std::result::Result<(), rodio::source::SeekError> {
        self.source.try_seek(position)
    }
}

/// Freezes the complete endpoint mix at one shared sample boundary.
///
/// This wrapper is placed after the mixer and before metering/frame counting.
/// While paused it emits silence without polling the mixer, so no individual
/// Cue source advances. Because the wrapper is shared by the whole endpoint,
/// Pause All / Resume All cannot introduce per-Sink start/stop offsets.
struct OutputTransportGateSource<S> {
    source: S,
    paused: Arc<AtomicBool>,
    channels: usize,
    channel_index: usize,
    frame_paused: bool,
}

impl<S> OutputTransportGateSource<S>
where
    S: Source<Item = f32>,
{
    fn new(source: S, paused: Arc<AtomicBool>) -> Self {
        let channels = source.channels().get().max(1) as usize;
        Self {
            source,
            paused,
            channels,
            channel_index: 0,
            frame_paused: false,
        }
    }
}

impl<S> Iterator for OutputTransportGateSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        // Latch the transport state once per complete interleaved frame. A
        // Pause/Resume arriving between L and R must not split that frame.
        if self.channel_index == 0 {
            self.frame_paused = self.paused.load(Ordering::Acquire);
        }

        let sample = if self.frame_paused {
            // Do not advance the mixer while the transport is globally paused.
            0.0
        } else {
            self.source.next()?
        };

        self.channel_index += 1;
        if self.channel_index >= self.channels {
            self.channel_index = 0;
        }
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // The gate can emit silence indefinitely while paused, independent of
        // the wrapped mixer's current size hint.
        (0, None)
    }
}

impl<S> Source for OutputTransportGateSource<S>
where
    S: Source<Item = f32>,
{
    fn current_span_len(&self) -> Option<usize> {
        if self.frame_paused {
            None
        } else {
            self.source.current_span_len()
        }
    }

    fn channels(&self) -> std::num::NonZeroU16 {
        self.source.channels()
    }

    fn sample_rate(&self) -> std::num::NonZeroU32 {
        self.source.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.source.total_duration()
    }

    fn try_seek(
        &mut self,
        position: Duration,
    ) -> std::result::Result<(), rodio::source::SeekError> {
        self.channel_index = 0;
        self.frame_paused = self.paused.load(Ordering::Acquire);
        self.source.try_seek(position)
    }
}

const TIME_PITCH_FFT_SIZE: usize = 1024;
const TIME_PITCH_HOP: usize = TIME_PITCH_FFT_SIZE / 4;

fn automation_value_at(
    position_secs: f64,
    points: &[[f64; 2]],
    enabled: bool,
    fallback: f32,
) -> f32 {
    if !enabled || points.is_empty() {
        return fallback;
    }
    if position_secs <= points[0][0] {
        return points[0][1] as f32;
    }
    for pair in points.windows(2) {
        let left = pair[0];
        let right = pair[1];
        if position_secs <= right[0] {
            let span = (right[0] - left[0]).max(0.000_001);
            let t = ((position_secs - left[0]) / span).clamp(0.0, 1.0);
            return (left[1] + (right[1] - left[1]) * t) as f32;
        }
    }
    points
        .last()
        .map(|point| point[1] as f32)
        .unwrap_or(fallback)
}

fn sanitize_time_pitch_settings(settings: &mut TimePitchSettings) {
    if !settings.speed.is_finite() {
        settings.speed = 1.0;
    }
    settings.speed = settings.speed.clamp(0.1, 4.0);
    if !settings.pitch_cents.is_finite() {
        settings.pitch_cents = 0.0;
    }
    settings.pitch_cents = settings.pitch_cents.clamp(-2400.0, 2400.0);
    settings
        .speed_automation
        .retain(|point| point[0].is_finite() && point[1].is_finite());
    settings
        .speed_automation
        .sort_by(|a, b| a[0].total_cmp(&b[0]));
    for point in &mut settings.speed_automation {
        point[0] = point[0].max(0.0);
        point[1] = point[1].clamp(0.1, 4.0);
    }
    settings
        .pitch_automation
        .retain(|point| point[0].is_finite() && point[1].is_finite());
    settings
        .pitch_automation
        .sort_by(|a, b| a[0].total_cmp(&b[0]));
    for point in &mut settings.pitch_automation {
        point[0] = point[0].max(0.0);
        point[1] = point[1].clamp(-2400.0, 2400.0);
    }
}

/// Streaming phase-vocoder transform. Playback speed and pitch are independent:
/// speed changes the analysis hop, while pitch shifts spectral bins. This keeps
/// multichannel interleaving intact and supports automation without restarting a cue.

#[derive(Debug)]
struct LoopSeekFailure {
    input_frame: AtomicU64,
    media_frame: AtomicU64,
}

/// Maps the monotonically increasing decoded-frame index to the file's media
/// timeline. This lets automation and the UI playhead wrap with the audio while
/// the PCM stream itself remains continuous.
#[derive(Clone)]
struct LoopTimeline {
    initial_start_frame: u64,
    loop_start_frame: u64,
    loop_end_frame: u64,
    initial_loops_remaining: i32,
    failure: Arc<LoopSeekFailure>,
}

impl LoopTimeline {
    fn media_frame_at_input_frame(&self, input_frame: f64) -> f64 {
        let input_frame = input_frame.max(0.0);
        let failure_at = self.failure.input_frame.load(Ordering::Acquire);
        if failure_at != u64::MAX && input_frame >= failure_at as f64 {
            let failure_media = self.failure.media_frame.load(Ordering::Acquire) as f64;
            return failure_media + (input_frame - failure_at as f64);
        }

        let loop_len = self.loop_end_frame.saturating_sub(self.loop_start_frame) as f64;
        if loop_len <= 0.0 {
            return self.initial_start_frame as f64 + input_frame;
        }

        let first_pass_frames = self.loop_end_frame.saturating_sub(self.initial_start_frame) as f64;
        if input_frame < first_pass_frames {
            return self.initial_start_frame as f64 + input_frame;
        }

        let after_first_pass = input_frame - first_pass_frames;
        if self.initial_loops_remaining == -1 {
            return self.loop_start_frame as f64 + after_first_pass.rem_euclid(loop_len);
        }

        // loop_count is the total number of passes, including the first pass.
        // Therefore count=2 means one additional traversal of the loop region.
        let additional_passes = self.initial_loops_remaining.saturating_sub(1).max(0) as f64;
        let repeated_frames = loop_len * additional_passes;
        if after_first_pass < repeated_frames {
            self.loop_start_frame as f64 + after_first_pass.rem_euclid(loop_len)
        } else {
            self.loop_end_frame as f64 + (after_first_pass - repeated_frames)
        }
    }

    fn media_secs_at_input_frame(&self, input_frame: f64, sample_rate: f64) -> f64 {
        self.media_frame_at_input_frame(input_frame) / sample_rate.max(1.0)
    }

    fn loops_remaining_at_input_frame(&self, input_frame: f64) -> i32 {
        let input_frame = input_frame.max(0.0);
        let failure_at = self.failure.input_frame.load(Ordering::Acquire);
        if failure_at != u64::MAX && input_frame >= failure_at as f64 {
            return 1;
        }
        if self.initial_loops_remaining == -1 {
            return -1;
        }

        let first_pass_frames = self.loop_end_frame.saturating_sub(self.initial_start_frame) as f64;
        if input_frame < first_pass_frames {
            return self.initial_loops_remaining;
        }

        let loop_len = self.loop_end_frame.saturating_sub(self.loop_start_frame) as f64;
        if loop_len <= 0.0 {
            return 1;
        }
        let after_first_pass = input_frame - first_pass_frames;
        let completed_additional_passes = (after_first_pass / loop_len).floor() as i32 + 1;
        self.initial_loops_remaining
            .saturating_sub(completed_additional_passes)
            .max(1)
    }
}

fn should_apply_loop_fade_out(
    loop_runtime_active: bool,
    rendered_loops_remaining: i32,
    end_secs: f64,
    loop_end_secs: f64,
) -> bool {
    !loop_runtime_active
        || rendered_loops_remaining == 1
        || (end_secs > 0.0 && end_secs < loop_end_secs)
}

fn seconds_to_source_frame(seconds: f64, sample_rate: u32) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    let frames = (seconds * sample_rate.max(1) as f64).round();
    if !frames.is_finite() || frames <= 0.0 {
        0
    } else {
        frames.min(u64::MAX as f64) as u64
    }
}

/// A seekable decoder wrapper that emits a continuous sequence of source frames.
/// Loop boundaries are exclusive at loop_end and inclusive at loop_start.
/// `loop_count` is the total number of passes; -1 means infinite.
struct LoopingSource<S> {
    source: S,
    channels: usize,
    sample_rate: u32,
    initial_start_frame: u64,
    initial_seek_done: bool,
    media_frame_position: u64,
    channel_index: usize,
    frames_emitted: u64,
    loop_start_frame: u64,
    loop_end_frame: u64,
    loop_active: bool,
    source_failed: bool,
    loops_remaining: Arc<AtomicI32>,
    loop_iterations: Arc<AtomicU64>,
    timeline: Option<LoopTimeline>,
    file_label: String,
}

impl<S> LoopingSource<S>
where
    S: Source<Item = f32>,
{
    fn new(
        source: S,
        offset_secs: f64,
        loop_start_secs: f64,
        loop_end_secs: f64,
        loop_enabled: bool,
        loops_remaining: Arc<AtomicI32>,
        loop_iterations: Arc<AtomicU64>,
        file_label: String,
    ) -> Self {
        let channels = source.channels().get().max(1) as usize;
        let sample_rate = source.sample_rate().get().max(1);
        let initial_start_frame = seconds_to_source_frame(offset_secs, sample_rate);
        let loop_start_frame = seconds_to_source_frame(loop_start_secs, sample_rate);
        let loop_end_frame = seconds_to_source_frame(loop_end_secs, sample_rate);
        let initial_loops_remaining = loops_remaining.load(Ordering::Acquire);
        let loop_active = loop_enabled
            && loop_end_frame > loop_start_frame
            && (initial_loops_remaining == -1 || initial_loops_remaining > 1);
        let timeline = loop_active.then(|| LoopTimeline {
            initial_start_frame,
            loop_start_frame,
            loop_end_frame,
            initial_loops_remaining,
            failure: Arc::new(LoopSeekFailure {
                input_frame: AtomicU64::new(u64::MAX),
                media_frame: AtomicU64::new(0),
            }),
        });

        if timeline.is_some() {
            info!(
                "AUDIO_LOOP_SOURCE_ARMED: file='{}', offset_frame={}, loop_start_frame={}, loop_end_frame={}, loop_count={}, source_rate={} Hz, channels={}",
                file_label,
                initial_start_frame,
                loop_start_frame,
                loop_end_frame,
                initial_loops_remaining,
                sample_rate,
                channels
            );
        }

        Self {
            source,
            channels,
            sample_rate,
            initial_start_frame,
            initial_seek_done: false,
            media_frame_position: 0,
            channel_index: 0,
            frames_emitted: 0,
            loop_start_frame,
            loop_end_frame,
            loop_active,
            source_failed: false,
            loops_remaining,
            loop_iterations,
            timeline,
            file_label,
        }
    }

    fn initialize_decoder_position(&mut self) -> bool {
        if self.initial_seek_done {
            return !self.source_failed;
        }
        self.initial_seek_done = true;

        if self.initial_start_frame == 0 {
            self.media_frame_position = 0;
            return true;
        }

        let target = Duration::from_secs_f64(
            self.initial_start_frame as f64 / self.sample_rate.max(1) as f64,
        );
        match self.source.try_seek(target) {
            Ok(()) => {
                self.media_frame_position = self.initial_start_frame;
                true
            }
            Err(error) if error.source_intact() => {
                // Some decoders do not support random access. Preserve the old
                // skip-duration behavior by decoding forward on this producer thread.
                let samples_to_skip = self
                    .initial_start_frame
                    .saturating_mul(self.channels as u64);
                for _ in 0..samples_to_skip {
                    if self.source.next().is_none() {
                        self.source_failed = true;
                        warn!(
                            "AUDIO_SOURCE_OFFSET_UNAVAILABLE: file='{}', requested_frame={}, reason='{}'",
                            self.file_label, self.initial_start_frame, error
                        );
                        return false;
                    }
                }
                self.media_frame_position = self.initial_start_frame;
                true
            }
            Err(error) => {
                self.source_failed = true;
                error!(
                    "AUDIO_SOURCE_OFFSET_FAILED: file='{}', requested_frame={}, reason='{}'",
                    self.file_label, self.initial_start_frame, error
                );
                false
            }
        }
    }

    fn begin_next_loop_pass(&mut self) {
        if !self.loop_active || self.media_frame_position < self.loop_end_frame {
            return;
        }

        let remaining = self.loops_remaining.load(Ordering::Acquire);
        if remaining != -1 && remaining <= 1 {
            self.loop_active = false;
            return;
        }

        let target =
            Duration::from_secs_f64(self.loop_start_frame as f64 / self.sample_rate.max(1) as f64);
        match self.source.try_seek(target) {
            Ok(()) => {
                if remaining > 1 {
                    self.loops_remaining.fetch_sub(1, Ordering::AcqRel);
                }
                self.media_frame_position = self.loop_start_frame;
                self.loop_iterations.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) if error.source_intact() => {
                if let Some(timeline) = &self.timeline {
                    timeline
                        .failure
                        .media_frame
                        .store(self.media_frame_position, Ordering::Relaxed);
                    timeline
                        .failure
                        .input_frame
                        .store(self.frames_emitted, Ordering::Release);
                }
                self.loops_remaining.store(1, Ordering::Release);
                self.loop_active = false;
                warn!(
                    "AUDIO_LOOP_SEEK_UNSUPPORTED: file='{}', loop_start_frame={}, loop_end_frame={}, reason='{}'; playback will continue without further repeats",
                    self.file_label,
                    self.loop_start_frame,
                    self.loop_end_frame,
                    error
                );
            }
            Err(error) => {
                if let Some(timeline) = &self.timeline {
                    timeline
                        .failure
                        .media_frame
                        .store(self.media_frame_position, Ordering::Relaxed);
                    timeline
                        .failure
                        .input_frame
                        .store(self.frames_emitted, Ordering::Release);
                }
                self.loops_remaining.store(1, Ordering::Release);
                self.loop_active = false;
                self.source_failed = true;
                error!(
                    "AUDIO_LOOP_SEEK_FAILED: file='{}', loop_start_frame={}, loop_end_frame={}, reason='{}'; decoder can no longer continue safely",
                    self.file_label,
                    self.loop_start_frame,
                    self.loop_end_frame,
                    error
                );
            }
        }
    }
}

impl<S> Iterator for LoopingSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.initialize_decoder_position() || self.source_failed {
            return None;
        }

        if self.channel_index == 0 {
            self.begin_next_loop_pass();
            if self.source_failed {
                return None;
            }
        }

        let sample = self.source.next()?;
        self.channel_index += 1;
        if self.channel_index >= self.channels {
            self.channel_index = 0;
            self.media_frame_position = self.media_frame_position.saturating_add(1);
            self.frames_emitted = self.frames_emitted.saturating_add(1);
        }
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

impl<S> Source for LoopingSource<S>
where
    S: Source<Item = f32>,
{
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> std::num::NonZeroU16 {
        self.source.channels()
    }

    fn sample_rate(&self) -> std::num::NonZeroU32 {
        self.source.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        if self.timeline.is_some() {
            None
        } else {
            self.source.total_duration()
        }
    }
}

struct SpeedPitchSource<S> {
    source: S,
    channels: usize,
    sample_rate: f64,
    start_secs: f64,
    input_samples: VecDeque<f32>,
    input_start_frame: u64,
    input_cursor: f64,
    previous_input_cursor: Option<f64>,
    input_eof: bool,
    flush_remaining: usize,
    finished: bool,
    passthrough: bool,
    settings_neutral: bool,
    last_speed: f32,
    last_pitch_cents: f32,
    last_pitch_shift_changes_speed: bool,
    last_preserve_pitch_on_speed_change: bool,
    passthrough_sample_index: u64,
    /// Cumulative decoded input frame at the start of the current output hop.
    output_hop_start_input_cursor: f64,
    output_hop_speed: f32,
    loop_timeline: Option<LoopTimeline>,
    producer_loop_remaining_state: Arc<AtomicI32>,
    output_hop_sample_index: u64,
    output_samples: VecDeque<f32>,
    input_frame: Vec<Complex<f32>>,
    window: Vec<f32>,
    overlap: Vec<Vec<f32>>,
    overlap_norm: Vec<f32>,
    spectra: Vec<Vec<Complex<f32>>>,
    output_spectra: Vec<Vec<Complex<f32>>>,
    previous_phase: Vec<Vec<f32>>,
    synthesis_phase: Vec<Vec<f32>>,
    instantaneous_frequency: Vec<Vec<f32>>,
    phase_initialized: bool,
    fft_forward: Arc<dyn Fft<f32>>,
    fft_inverse: Arc<dyn Fft<f32>>,
    settings: Arc<RwLock<TimePitchSettings>>,
    position_state: Arc<AtomicU64>,
}

impl<S> SpeedPitchSource<S>
where
    S: Source<Item = f32>,
{
    #[cfg(test)]
    fn new(
        source: S,
        start_secs: f64,
        settings: Arc<RwLock<TimePitchSettings>>,
        position_state: Arc<AtomicU64>,
    ) -> Self {
        Self::new_with_loop(
            source,
            start_secs,
            settings,
            position_state,
            None,
            Arc::new(AtomicI32::new(0)),
        )
    }

    fn new_with_loop(
        source: S,
        start_secs: f64,
        settings: Arc<RwLock<TimePitchSettings>>,
        position_state: Arc<AtomicU64>,
        loop_timeline: Option<LoopTimeline>,
        producer_loop_remaining_state: Arc<AtomicI32>,
    ) -> Self {
        let channels = source.channels().get().max(1) as usize;
        let sample_rate = source.sample_rate().get().max(1) as f64;
        let mut planner = FftPlanner::<f32>::new();
        let fft_forward = planner.plan_fft_forward(TIME_PITCH_FFT_SIZE);
        let fft_inverse = planner.plan_fft_inverse(TIME_PITCH_FFT_SIZE);
        let window = (0..TIME_PITCH_FFT_SIZE)
            .map(|index| {
                let phase = std::f32::consts::TAU * index as f32 / (TIME_PITCH_FFT_SIZE - 1) as f32;
                0.54 - 0.46 * phase.cos()
            })
            .collect();
        let bins = TIME_PITCH_FFT_SIZE;
        let initial_media_position = loop_timeline
            .as_ref()
            .map(|timeline| timeline.media_secs_at_input_frame(0.0, sample_rate))
            .unwrap_or_else(|| start_secs.max(0.0));
        atomic_store_f64(&position_state, initial_media_position);
        if let Some(timeline) = &loop_timeline {
            producer_loop_remaining_state.store(
                timeline.loops_remaining_at_input_frame(0.0),
                Ordering::Release,
            );
        }
        let (
            settings_neutral,
            last_speed,
            last_pitch_cents,
            last_pitch_shift_changes_speed,
            last_preserve_pitch_on_speed_change,
        ) = {
            let initial = settings.read();
            (
                !initial.speed_automation_enabled
                    && !initial.pitch_automation_enabled
                    && (initial.speed - 1.0).abs() < 0.0001
                    && initial.pitch_cents.abs() < 0.0001,
                initial.speed,
                initial.pitch_cents,
                initial.pitch_shift_changes_speed,
                initial.preserve_pitch_on_speed_change,
            )
        };
        let passthrough = settings_neutral;
        Self {
            source,
            channels,
            sample_rate,
            start_secs,
            input_samples: VecDeque::with_capacity(TIME_PITCH_FFT_SIZE * channels * 2),
            input_start_frame: 0,
            input_cursor: 0.0,
            previous_input_cursor: None,
            input_eof: false,
            flush_remaining: 0,
            finished: false,
            passthrough,
            settings_neutral,
            last_speed,
            last_pitch_cents,
            last_pitch_shift_changes_speed,
            last_preserve_pitch_on_speed_change,
            passthrough_sample_index: 0,
            output_hop_start_input_cursor: 0.0,
            output_hop_speed: 1.0,
            loop_timeline,
            producer_loop_remaining_state,
            output_hop_sample_index: 0,
            output_samples: VecDeque::with_capacity(TIME_PITCH_HOP * channels),
            input_frame: vec![Complex::new(0.0, 0.0); TIME_PITCH_FFT_SIZE],
            window,
            overlap: vec![vec![0.0; TIME_PITCH_FFT_SIZE]; channels],
            overlap_norm: vec![0.0; TIME_PITCH_FFT_SIZE],
            spectra: vec![vec![Complex::new(0.0, 0.0); bins]; channels],
            output_spectra: vec![vec![Complex::new(0.0, 0.0); bins]; channels],
            previous_phase: vec![vec![0.0; bins]; channels],
            synthesis_phase: vec![vec![0.0; bins]; channels],
            instantaneous_frequency: vec![vec![0.0; bins]; channels],
            phase_initialized: false,
            fft_forward,
            fft_inverse,
            settings,
            position_state,
        }
    }

    fn media_position_at_input_frame(&self, input_frame: f64) -> f64 {
        self.loop_timeline
            .as_ref()
            .map(|timeline| timeline.media_secs_at_input_frame(input_frame, self.sample_rate))
            .unwrap_or_else(|| self.start_secs + input_frame.max(0.0) / self.sample_rate)
    }

    fn available_end_frame(&self) -> u64 {
        self.input_start_frame + (self.input_samples.len() / self.channels) as u64
    }

    fn fill_input_until(&mut self, required_end_frame: u64) {
        while !self.input_eof && self.available_end_frame() < required_end_frame {
            // This method runs from the audio source's `next()` path. Read directly
            // into the preallocated VecDeque instead of allocating a temporary Vec
            // for every interleaved frame.
            let mut samples_read = 0;
            for _ in 0..self.channels {
                match self.source.next() {
                    Some(sample) => {
                        self.input_samples.push_back(sample);
                        samples_read += 1;
                    }
                    None => {
                        self.input_eof = true;
                        break;
                    }
                }
            }

            if samples_read == 0 {
                break;
            }

            // Preserve the previous behavior for a decoder that ends partway
            // through an interleaved frame: pad the final frame with silence.
            while samples_read < self.channels {
                self.input_samples.push_back(0.0);
                samples_read += 1;
            }
        }
    }

    fn sample_at(&self, frame: u64, channel: usize) -> f32 {
        if frame < self.input_start_frame || frame >= self.available_end_frame() {
            return 0.0;
        }
        let offset = (frame - self.input_start_frame) as usize * self.channels + channel;
        self.input_samples.get(offset).copied().unwrap_or(0.0)
    }

    fn interpolated_sample(&self, frame_position: f64, channel: usize) -> f32 {
        let base = frame_position.floor().max(0.0) as u64;
        let fraction = (frame_position - base as f64).clamp(0.0, 1.0) as f32;
        let first = self.sample_at(base, channel);
        let second = self.sample_at(base.saturating_add(1), channel);
        first + (second - first) * fraction
    }

    fn discard_consumed_input(&mut self) {
        let keep_from = (self.input_cursor.floor().max(0.0) as u64).saturating_sub(1);
        while self.input_start_frame < keep_from && self.input_samples.len() >= self.channels {
            for _ in 0..self.channels {
                self.input_samples.pop_front();
            }
            self.input_start_frame += 1;
        }
    }

    fn shift_overlap(&mut self) {
        for channel in 0..self.channels {
            self.overlap[channel].copy_within(TIME_PITCH_HOP..TIME_PITCH_FFT_SIZE, 0);
            self.overlap[channel][TIME_PITCH_FFT_SIZE - TIME_PITCH_HOP..].fill(0.0);
        }
        self.overlap_norm
            .copy_within(TIME_PITCH_HOP..TIME_PITCH_FFT_SIZE, 0);
        self.overlap_norm[TIME_PITCH_FFT_SIZE - TIME_PITCH_HOP..].fill(0.0);
    }

    fn emit_overlap_hop(&mut self) {
        for frame in 0..TIME_PITCH_HOP {
            let normalization = self.overlap_norm[frame].max(0.000_01);
            for channel in 0..self.channels {
                self.output_samples
                    .push_back(self.overlap[channel][frame] / normalization);
            }
        }
        self.shift_overlap();
    }

    fn generate_hop(&mut self) {
        if self.finished {
            return;
        }

        let required_end =
            self.input_cursor.floor().max(0.0) as u64 + TIME_PITCH_FFT_SIZE as u64 + 1;
        self.fill_input_until(required_end);
        let available_end = self.available_end_frame();

        if self.input_eof && available_end == 0 {
            atomic_store_f64(&self.position_state, self.start_secs);
            self.finished = true;
            return;
        }

        if self.input_eof && self.input_cursor >= available_end as f64 {
            if self.flush_remaining == 0 {
                self.flush_remaining = TIME_PITCH_FFT_SIZE / TIME_PITCH_HOP - 1;
            }
            if self.flush_remaining == 0 {
                self.finished = true;
                return;
            }
            self.emit_overlap_hop();
            self.flush_remaining -= 1;
            if self.flush_remaining == 0 {
                // The final overlap hop has been queued. `next` publishes the
                // final media position only after that queued audio is consumed.
                self.finished = true;
            }
            return;
        }

        let media_time = self.media_position_at_input_frame(self.input_cursor);
        // Never wait for a UI/control-thread settings write from the audio
        // source. If a writer currently owns the lock, reuse the last complete
        // scalar settings for this analysis hop.
        let refreshed_settings = self.settings.try_read().map(|settings| {
            let speed = automation_value_at(
                media_time,
                &settings.speed_automation,
                settings.speed_automation_enabled,
                settings.speed,
            )
            .clamp(0.1, 4.0);
            let pitch_cents = automation_value_at(
                media_time,
                &settings.pitch_automation,
                settings.pitch_automation_enabled,
                settings.pitch_cents,
            )
            .clamp(-2400.0, 2400.0);
            (
                speed,
                pitch_cents,
                settings.pitch_shift_changes_speed,
                settings.preserve_pitch_on_speed_change,
            )
        });
        let (speed, pitch_cents, pitch_shift_changes_speed, preserve_pitch_on_speed_change) =
            if let Some(refreshed) = refreshed_settings {
                self.last_speed = refreshed.0;
                self.last_pitch_cents = refreshed.1;
                self.last_pitch_shift_changes_speed = refreshed.2;
                self.last_preserve_pitch_on_speed_change = refreshed.3;
                refreshed
            } else {
                (
                    self.last_speed,
                    self.last_pitch_cents,
                    self.last_pitch_shift_changes_speed,
                    self.last_preserve_pitch_on_speed_change,
                )
            };
        let pitch_factor = 2.0_f32.powf(pitch_cents / 1200.0);
        let effective_speed = (speed
            * if pitch_shift_changes_speed {
                pitch_factor
            } else {
                1.0
            })
        .clamp(0.1, 4.0);
        let effective_pitch = pitch_factor
            * if preserve_pitch_on_speed_change {
                1.0
            } else {
                speed
            };
        let effective_pitch = effective_pitch.clamp(0.25, 4.0);
        let analysis_hop = self
            .previous_input_cursor
            .map(|previous| (self.input_cursor - previous).max(0.001))
            .unwrap_or(TIME_PITCH_HOP as f64 * effective_speed as f64);

        self.output_hop_start_input_cursor = self.input_cursor;
        self.output_hop_speed = effective_speed;
        self.output_hop_sample_index = 0;
        atomic_store_f64(&self.position_state, media_time);

        let forward = self.fft_forward.clone();
        let inverse = self.fft_inverse.clone();
        let half = TIME_PITCH_FFT_SIZE / 2;
        let pitch = effective_pitch;

        let phase_initialized = self.phase_initialized;
        for channel in 0..self.channels {
            for index in 0..TIME_PITCH_FFT_SIZE {
                let sample = self.interpolated_sample(self.input_cursor + index as f64, channel);
                self.input_frame[index] = Complex::new(sample * self.window[index], 0.0);
            }
            self.spectra[channel].copy_from_slice(&self.input_frame);
            let spectrum = &mut self.spectra[channel];
            forward.process(spectrum);

            let previous_phase = &mut self.previous_phase[channel];
            let instant_frequency = &mut self.instantaneous_frequency[channel];
            let synthesis_phase = &mut self.synthesis_phase[channel];
            for bin in 0..=half {
                let phase = spectrum[bin].arg();
                if phase_initialized {
                    let expected = std::f32::consts::TAU
                        * bin as f32
                        * (analysis_hop as f32 / TIME_PITCH_FFT_SIZE as f32);
                    let mut deviation = phase - previous_phase[bin] - expected;
                    while deviation > std::f32::consts::PI {
                        deviation -= std::f32::consts::TAU;
                    }
                    while deviation < -std::f32::consts::PI {
                        deviation += std::f32::consts::TAU;
                    }
                    instant_frequency[bin] = std::f32::consts::TAU * bin as f32
                        / TIME_PITCH_FFT_SIZE as f32
                        + deviation / analysis_hop as f32;
                } else {
                    instant_frequency[bin] =
                        std::f32::consts::TAU * bin as f32 / TIME_PITCH_FFT_SIZE as f32;
                }
                previous_phase[bin] = phase;
            }

            let output_spectrum = &mut self.output_spectra[channel];
            output_spectrum.fill(Complex::new(0.0, 0.0));
            for output_bin in 0..=half {
                let source_bin = output_bin as f32 / pitch;
                if source_bin > half as f32 {
                    continue;
                }
                let low = source_bin.floor() as usize;
                let high = (low + 1).min(half);
                let fraction = source_bin - low as f32;
                let low_magnitude = spectrum[low].norm();
                let high_magnitude = spectrum[high].norm();
                let magnitude = low_magnitude + (high_magnitude - low_magnitude) * fraction;
                let omega = instant_frequency[low]
                    + (instant_frequency[high] - instant_frequency[low]) * fraction;

                if phase_initialized {
                    synthesis_phase[output_bin] += omega * pitch * TIME_PITCH_HOP as f32;
                } else {
                    let mut phase_delta = spectrum[high].arg() - spectrum[low].arg();
                    while phase_delta > std::f32::consts::PI {
                        phase_delta -= std::f32::consts::TAU;
                    }
                    while phase_delta < -std::f32::consts::PI {
                        phase_delta += std::f32::consts::TAU;
                    }
                    synthesis_phase[output_bin] = spectrum[low].arg() + phase_delta * fraction;
                }
                output_spectrum[output_bin] =
                    Complex::from_polar(magnitude, synthesis_phase[output_bin]);
            }
            for bin in 1..half {
                output_spectrum[TIME_PITCH_FFT_SIZE - bin] = output_spectrum[bin].conj();
            }
            output_spectrum[0].im = 0.0;
            output_spectrum[half].im = 0.0;
            inverse.process(output_spectrum);

            for index in 0..TIME_PITCH_FFT_SIZE {
                self.overlap[channel][index] +=
                    output_spectrum[index].re / TIME_PITCH_FFT_SIZE as f32 * self.window[index];
            }
        }

        for index in 0..TIME_PITCH_FFT_SIZE {
            self.overlap_norm[index] += self.window[index] * self.window[index];
        }
        self.phase_initialized = true;
        self.previous_input_cursor = Some(self.input_cursor);
        self.emit_overlap_hop();
        self.input_cursor += TIME_PITCH_HOP as f64 * effective_speed as f64;
        self.discard_consumed_input();
    }
}

impl<S> Iterator for SpeedPitchSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.passthrough {
            let at_block_boundary =
                self.passthrough_sample_index % (TIME_PITCH_HOP * self.channels) as u64 == 0;
            if at_block_boundary {
                let refreshed_neutral = self.settings.try_read().map(|settings| {
                    !settings.speed_automation_enabled
                        && !settings.pitch_automation_enabled
                        && (settings.speed - 1.0).abs() < 0.0001
                        && settings.pitch_cents.abs() < 0.0001
                });
                if let Some(still_neutral) = refreshed_neutral {
                    self.settings_neutral = still_neutral;
                }
                if !self.settings_neutral {
                    let consumed_frames = self.passthrough_sample_index / self.channels as u64;
                    self.passthrough_sample_index = 0;
                    self.input_start_frame = consumed_frames;
                    self.input_cursor = consumed_frames as f64;
                    self.output_hop_start_input_cursor = self.input_cursor;
                    self.previous_input_cursor = None;
                    self.input_eof = false;
                    self.flush_remaining = 0;
                    self.finished = false;
                    self.phase_initialized = false;
                    let current_media_position =
                        self.media_position_at_input_frame(self.input_cursor);
                    atomic_store_f64(&self.position_state, current_media_position);
                    self.passthrough = false;
                }
            }
            if self.passthrough {
                let sample = self.source.next();
                if sample.is_some() {
                    if self.passthrough_sample_index % self.channels as u64 == 0 {
                        let frame = self.passthrough_sample_index / self.channels as u64;
                        let input_frame = (frame + 1) as f64;
                        let media_position = self.media_position_at_input_frame(input_frame);
                        atomic_store_f64(&self.position_state, media_position);
                        let loops_remaining = self
                            .loop_timeline
                            .as_ref()
                            .map(|timeline| timeline.loops_remaining_at_input_frame(input_frame))
                            .unwrap_or(0);
                        self.producer_loop_remaining_state
                            .store(loops_remaining, Ordering::Release);
                    }
                    self.passthrough_sample_index = self.passthrough_sample_index.wrapping_add(1);
                } else {
                    let frames = self.passthrough_sample_index / self.channels as u64;
                    let media_position = self.media_position_at_input_frame(frames as f64);
                    atomic_store_f64(&self.position_state, media_position);
                    self.finished = true;
                }
                return sample;
            }
        }

        while self.output_samples.is_empty() && !self.finished {
            self.generate_hop();
        }
        let sample = self.output_samples.pop_front();
        if sample.is_some() {
            if self.output_hop_sample_index % self.channels as u64 == 0 {
                let frame_index = self.output_hop_sample_index / self.channels as u64;
                let input_frame = self.output_hop_start_input_cursor
                    + (frame_index + 1) as f64 * self.output_hop_speed as f64;
                let media_position = self.media_position_at_input_frame(input_frame);
                atomic_store_f64(&self.position_state, media_position);
                let loops_remaining = self
                    .loop_timeline
                    .as_ref()
                    .map(|timeline| timeline.loops_remaining_at_input_frame(input_frame))
                    .unwrap_or(0);
                self.producer_loop_remaining_state
                    .store(loops_remaining, Ordering::Release);
            }
            self.output_hop_sample_index = self.output_hop_sample_index.wrapping_add(1);
        }
        if self.finished && self.output_samples.is_empty() {
            let final_position =
                self.media_position_at_input_frame(self.available_end_frame() as f64);
            atomic_store_f64(&self.position_state, final_position);
        }
        sample
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

impl<S> Source for SpeedPitchSource<S>
where
    S: Source<Item = f32>,
{
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> std::num::NonZeroU16 {
        self.source.channels()
    }

    fn sample_rate(&self) -> std::num::NonZeroU32 {
        self.source.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

const PCM_PREFETCH_CHUNK_FRAMES: usize = 512;
const PCM_PREFETCH_BUFFER_COUNT: usize = 8;
const PCM_LOOP_FINGERPRINT_FRAMES: usize = 128;
const PCM_FNV1A64_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const PCM_FNV1A64_PRIME: u64 = 0x100000001b3;

// Keep the loop-boundary sample capture fixed-size and lock-free in the render
// path. Four source frames before and eight after each rendered wrap are enough
// to identify whether the PCM itself changes at the boundary.
const PCM_LOOP_CAPTURE_PRE_FRAMES: usize = 4;
const PCM_LOOP_CAPTURE_POST_FRAMES: usize = 8;
const PCM_LOOP_CAPTURE_MAX_CHANNELS: usize = 32;
const PCM_LOOP_CAPTURE_SAMPLE_COUNT: usize =
    (PCM_LOOP_CAPTURE_PRE_FRAMES + PCM_LOOP_CAPTURE_POST_FRAMES) * PCM_LOOP_CAPTURE_MAX_CHANNELS;
const PCM_LOOP_CAPTURE_STATUS_PENDING: u32 = 0;
const PCM_LOOP_CAPTURE_STATUS_VALID: u32 = 1;
const PCM_LOOP_CAPTURE_STATUS_UNDERRUN: u32 = 2;
const PCM_LOOP_CAPTURE_STATUS_UNSUPPORTED_CHANNELS: u32 = 3;
const PCM_LOOP_CAPTURE_STATUS_INCOMPLETE: u32 = 4;

/// Fixed-size handoff from the render source to the Worker control loop.
/// The render source writes atomics only; formatting and logging happen on
/// the Worker thread.
struct LoopPcmCapture {
    samples: [AtomicU32; PCM_LOOP_CAPTURE_SAMPLE_COUNT],
    channels: AtomicU32,
    pre_frames_valid: AtomicU32,
    post_samples_captured: AtomicU32,
    iteration: AtomicU64,
    source_elapsed_frame: AtomicU64,
    source_media_frame: AtomicU64,
    endpoint_render_frame: AtomicU64,
    status: AtomicU32,
    completed_iteration: AtomicU64,
    acknowledged_iteration: AtomicU64,
    dropped_captures: AtomicU64,
}

impl LoopPcmCapture {
    fn new() -> Self {
        Self {
            samples: std::array::from_fn(|_| AtomicU32::new(0)),
            channels: AtomicU32::new(0),
            pre_frames_valid: AtomicU32::new(0),
            post_samples_captured: AtomicU32::new(0),
            iteration: AtomicU64::new(0),
            source_elapsed_frame: AtomicU64::new(0),
            source_media_frame: AtomicU64::new(0),
            endpoint_render_frame: AtomicU64::new(u64::MAX),
            status: AtomicU32::new(PCM_LOOP_CAPTURE_STATUS_PENDING),
            completed_iteration: AtomicU64::new(0),
            acknowledged_iteration: AtomicU64::new(0),
            dropped_captures: AtomicU64::new(0),
        }
    }
}

struct PcmPrefetchChunk {
    samples: Vec<f32>,
    /// Media position after each complete interleaved frame in `samples`.
    /// This preserves loop wraps and non-linear Time/Pitch movement inside a chunk.
    frame_end_positions: Vec<u64>,
    frame_loop_remaining: Vec<i32>,
    start_position_bits: u64,
    end_position_bits: u64,
    start_loop_remaining: i32,
    eof: bool,
}

static PCM_BUFFER_REAPER: OnceLock<Option<Sender<PcmPrefetchChunk>>> = OnceLock::new();

/// Buffers retired by a render-thread source drop are released by this helper
/// thread rather than deallocated inside the native audio callback.
fn pcm_buffer_reaper() -> Option<&'static Sender<PcmPrefetchChunk>> {
    PCM_BUFFER_REAPER
        .get_or_init(|| {
            let (sender, receiver) = bounded::<PcmPrefetchChunk>(2_048);
            match thread::Builder::new()
                .name("tasq-pcm-buffer-reaper".to_string())
                .spawn(move || {
                    while let Ok(chunk) = receiver.recv() {
                        drop(chunk);
                    }
                }) {
                Ok(_) => Some(sender),
                Err(error) => {
                    error!("Could not start PCM buffer reaper thread: {error}");
                    None
                }
            }
        })
        .as_ref()
}

fn retire_pcm_buffer(chunk: PcmPrefetchChunk) {
    if let Some(reaper) = pcm_buffer_reaper() {
        if let Err(error) = reaper.try_send(chunk) {
            // The reaper is normally available. If it has disconnected, leak
            // the complete chunk rather than free either Vec on the callback.
            std::mem::forget(error.into_inner());
        }
    } else {
        // Thread creation failure is exceptional. Avoid a render-thread free.
        std::mem::forget(chunk);
    }
}

fn prefetch_chunk_frame_start_position(chunk: &PcmPrefetchChunk, frame_index: usize) -> f64 {
    if frame_index == 0 {
        return f64::from_bits(chunk.start_position_bits);
    }
    chunk
        .frame_end_positions
        .get(frame_index - 1)
        .copied()
        .map(f64::from_bits)
        .unwrap_or_else(|| f64::from_bits(chunk.end_position_bits))
}

fn prefetch_chunk_frame_start_loop_remaining(chunk: &PcmPrefetchChunk, frame_index: usize) -> i32 {
    if frame_index == 0 {
        return chunk.start_loop_remaining;
    }
    chunk
        .frame_loop_remaining
        .get(frame_index - 1)
        .copied()
        .unwrap_or(chunk.start_loop_remaining)
}

/// SPSC-style chunk prefetch: decoding and time/pitch processing run on a
/// producer thread; the render thread only reads preallocated PCM chunks.
struct PcmPrefetchSource {
    channels: std::num::NonZeroU16,
    sample_rate: std::num::NonZeroU32,
    total_duration: Option<Duration>,
    ready_rx: Receiver<PcmPrefetchChunk>,
    /// The producer holds a guard receiver so queued chunks are drained there
    /// if the Cue is stopped while data remains buffered.
    free_tx: Sender<PcmPrefetchChunk>,
    current_chunk: Option<PcmPrefetchChunk>,
    current_index: usize,
    producer_finished: Arc<AtomicBool>,
    render_position_state: Arc<AtomicU64>,
    underrun_count: Arc<AtomicU64>,
    playback_elapsed_frames: Arc<AtomicU64>,
    render_loop_remaining_state: Arc<AtomicI32>,
    /// Number of complete source frames that had to be replaced by silence.
    underrun_frame_count: Arc<AtomicU64>,
    /// Remaining channels in the current underrun-silence frame. This prevents
    /// a newly arrived chunk from being consumed halfway through an interleaved frame.
    underrun_silence_samples_remaining: usize,
    underrun_active: bool,
    /// Render-side loop diagnostics. These are updated without logging or allocation.
    loop_diagnostics_enabled: bool,
    render_loop_iterations: Arc<AtomicU64>,
    render_loop_boundary_frame: Arc<AtomicU64>,
    render_loop_boundary_position_bits: Arc<AtomicU64>,
    render_loop_boundary_endpoint_frame: Arc<AtomicU64>,
    endpoint_frame_clock: Option<Arc<AtomicU64>>,
    render_loop_fingerprint: Arc<AtomicU64>,
    render_loop_fingerprint_iteration: Arc<AtomicU64>,
    loop_pcm_capture: Arc<LoopPcmCapture>,
    loop_capture_pre_samples: [f32; PCM_LOOP_CAPTURE_PRE_FRAMES * PCM_LOOP_CAPTURE_MAX_CHANNELS],
    loop_capture_pre_frames: usize,
    loop_capture_pre_write_frame: usize,
    loop_capture_active: bool,
    loop_capture_iteration: u64,
    loop_capture_post_samples_written: usize,
    last_render_media_position: Option<f64>,
    loop_fingerprint_active: bool,
    loop_fingerprint_samples_remaining: usize,
    loop_fingerprint_hash: u64,
    loop_fingerprint_iteration: u64,
    consumer_finished: bool,
}

impl PcmPrefetchSource {
    fn begin_loop_pcm_capture(
        &mut self,
        iteration: u64,
        source_elapsed_frame: u64,
        source_media_frame: u64,
        endpoint_render_frame: u64,
    ) {
        if self.loop_capture_active {
            self.invalidate_loop_pcm_capture(PCM_LOOP_CAPTURE_STATUS_INCOMPLETE);
        }
        let completed = self
            .loop_pcm_capture
            .completed_iteration
            .load(Ordering::Acquire);
        let acknowledged = self
            .loop_pcm_capture
            .acknowledged_iteration
            .load(Ordering::Acquire);
        if completed > acknowledged {
            // Do not overwrite a capture before the Worker has consumed it.
            self.loop_pcm_capture
                .dropped_captures
                .fetch_add(1, Ordering::Relaxed);
            return;
        }

        let channel_count = self.channels.get().max(1) as usize;
        self.loop_pcm_capture
            .channels
            .store(channel_count as u32, Ordering::Relaxed);
        self.loop_pcm_capture
            .pre_frames_valid
            .store(0, Ordering::Relaxed);
        self.loop_pcm_capture
            .post_samples_captured
            .store(0, Ordering::Relaxed);
        self.loop_pcm_capture
            .iteration
            .store(iteration, Ordering::Relaxed);
        self.loop_pcm_capture
            .source_elapsed_frame
            .store(source_elapsed_frame, Ordering::Relaxed);
        self.loop_pcm_capture
            .source_media_frame
            .store(source_media_frame, Ordering::Relaxed);
        self.loop_pcm_capture
            .endpoint_render_frame
            .store(endpoint_render_frame, Ordering::Relaxed);

        if channel_count > PCM_LOOP_CAPTURE_MAX_CHANNELS {
            self.loop_pcm_capture.status.store(
                PCM_LOOP_CAPTURE_STATUS_UNSUPPORTED_CHANNELS,
                Ordering::Relaxed,
            );
            self.loop_pcm_capture
                .completed_iteration
                .store(iteration, Ordering::Release);
            return;
        }

        let pre_frames = self
            .loop_capture_pre_frames
            .min(PCM_LOOP_CAPTURE_PRE_FRAMES);
        let missing_pre_frames = PCM_LOOP_CAPTURE_PRE_FRAMES - pre_frames;
        let oldest_pre_frame = if pre_frames == PCM_LOOP_CAPTURE_PRE_FRAMES {
            self.loop_capture_pre_write_frame
        } else {
            0
        };

        // Store the pre-boundary frames right-aligned so the post-boundary
        // region always starts at the same fixed offset in the shared buffer.
        for destination_frame in 0..PCM_LOOP_CAPTURE_PRE_FRAMES {
            for channel in 0..channel_count {
                let sample = if destination_frame < missing_pre_frames {
                    0.0
                } else {
                    let chronological_frame = destination_frame - missing_pre_frames;
                    let source_frame = if pre_frames == PCM_LOOP_CAPTURE_PRE_FRAMES {
                        (oldest_pre_frame + chronological_frame) % PCM_LOOP_CAPTURE_PRE_FRAMES
                    } else {
                        chronological_frame
                    };
                    self.loop_capture_pre_samples
                        [source_frame * PCM_LOOP_CAPTURE_MAX_CHANNELS + channel]
                };
                let sample_index = destination_frame * channel_count + channel;
                self.loop_pcm_capture.samples[sample_index]
                    .store(sample.to_bits(), Ordering::Relaxed);
            }
        }

        self.loop_pcm_capture
            .pre_frames_valid
            .store(pre_frames as u32, Ordering::Relaxed);
        self.loop_pcm_capture
            .status
            .store(PCM_LOOP_CAPTURE_STATUS_PENDING, Ordering::Relaxed);
        self.loop_capture_active = true;
        self.loop_capture_iteration = iteration;
        self.loop_capture_post_samples_written = 0;
    }

    fn invalidate_loop_pcm_capture(&mut self, status: u32) {
        if !self.loop_capture_active {
            return;
        }

        self.loop_pcm_capture.post_samples_captured.store(
            self.loop_capture_post_samples_written as u32,
            Ordering::Relaxed,
        );
        self.loop_pcm_capture
            .status
            .store(status, Ordering::Relaxed);
        self.loop_pcm_capture
            .completed_iteration
            .store(self.loop_capture_iteration, Ordering::Release);
        self.loop_capture_active = false;
    }

    fn record_loop_pcm_capture_sample(&mut self, sample: f32) {
        if !self.loop_diagnostics_enabled && !self.loop_capture_active {
            return;
        }
        let channel_count = self.channels.get().max(1) as usize;
        if channel_count > PCM_LOOP_CAPTURE_MAX_CHANNELS {
            return;
        }

        if self.loop_capture_active {
            let post_sample_count = PCM_LOOP_CAPTURE_POST_FRAMES * channel_count;
            if self.loop_capture_post_samples_written < post_sample_count {
                let sample_index = PCM_LOOP_CAPTURE_PRE_FRAMES * channel_count
                    + self.loop_capture_post_samples_written;
                self.loop_pcm_capture.samples[sample_index]
                    .store(sample.to_bits(), Ordering::Relaxed);
                self.loop_capture_post_samples_written += 1;

                if self.loop_capture_post_samples_written >= post_sample_count {
                    self.loop_pcm_capture.post_samples_captured.store(
                        self.loop_capture_post_samples_written as u32,
                        Ordering::Relaxed,
                    );
                    self.loop_pcm_capture
                        .status
                        .store(PCM_LOOP_CAPTURE_STATUS_VALID, Ordering::Relaxed);
                    self.loop_pcm_capture
                        .completed_iteration
                        .store(self.loop_capture_iteration, Ordering::Release);
                    self.loop_capture_active = false;
                }
            }
            return;
        }

        let channel_index = self.current_index % channel_count;
        let sample_index =
            self.loop_capture_pre_write_frame * PCM_LOOP_CAPTURE_MAX_CHANNELS + channel_index;
        self.loop_capture_pre_samples[sample_index] = sample;
        if channel_index + 1 >= channel_count {
            self.loop_capture_pre_write_frame =
                (self.loop_capture_pre_write_frame + 1) % PCM_LOOP_CAPTURE_PRE_FRAMES;
            self.loop_capture_pre_frames =
                (self.loop_capture_pre_frames + 1).min(PCM_LOOP_CAPTURE_PRE_FRAMES);
        }
    }

    fn new<S>(
        mut source: S,
        producer_position_state: Arc<AtomicU64>,
        render_position_state: Arc<AtomicU64>,
        underrun_count: Arc<AtomicU64>,
        playback_elapsed_frames: Arc<AtomicU64>,
        producer_loop_remaining_state: Arc<AtomicI32>,
        render_loop_remaining_state: Arc<AtomicI32>,
        underrun_frame_count: Arc<AtomicU64>,
        loop_diagnostics_enabled: bool,
        render_loop_iterations: Arc<AtomicU64>,
        render_loop_boundary_frame: Arc<AtomicU64>,
        render_loop_boundary_position_bits: Arc<AtomicU64>,
        render_loop_boundary_endpoint_frame: Arc<AtomicU64>,
        endpoint_frame_clock: Option<Arc<AtomicU64>>,
        render_loop_fingerprint: Arc<AtomicU64>,
        render_loop_fingerprint_iteration: Arc<AtomicU64>,
        loop_pcm_capture: Arc<LoopPcmCapture>,
        pump_during_wait: &mut dyn FnMut(),
    ) -> Option<Self>
    where
        S: Source<Item = f32> + Send + 'static,
    {
        // Initialize the retirement worker before the source can be dropped by
        // the native render callback.
        let _ = pcm_buffer_reaper();
        let channels = source.channels();
        let sample_rate = source.sample_rate();
        let total_duration = source.total_duration();
        let channel_count = channels.get().max(1) as usize;
        let chunk_capacity = PCM_PREFETCH_CHUNK_FRAMES.saturating_mul(channel_count);
        let (free_tx, free_rx) = bounded::<PcmPrefetchChunk>(PCM_PREFETCH_BUFFER_COUNT);
        let (ready_tx, ready_rx) = bounded::<PcmPrefetchChunk>(PCM_PREFETCH_BUFFER_COUNT);
        for _ in 0..PCM_PREFETCH_BUFFER_COUNT {
            if free_tx
                .send(PcmPrefetchChunk {
                    samples: Vec::with_capacity(chunk_capacity),
                    frame_end_positions: Vec::with_capacity(PCM_PREFETCH_CHUNK_FRAMES),
                    frame_loop_remaining: Vec::with_capacity(PCM_PREFETCH_CHUNK_FRAMES),
                    start_position_bits: 0.0_f64.to_bits(),
                    end_position_bits: 0.0_f64.to_bits(),
                    start_loop_remaining: 0,
                    eof: false,
                })
                .is_err()
            {
                return None;
            }
        }

        let producer_finished = Arc::new(AtomicBool::new(false));
        let producer_finished_thread = producer_finished.clone();
        let producer_position_state_thread = producer_position_state.clone();
        let producer_loop_remaining_state_thread = producer_loop_remaining_state.clone();
        // Keep the queue alive until the producer can drain it after the
        // consumer drops its receiver during Stop or natural completion.
        let ready_rx_guard = ready_rx.clone();
        let producer = thread::Builder::new()
            .name("tasq-pcm-prefetch".to_string())
            .spawn(move || {
                loop {
                    let mut chunk = match free_rx.recv() {
                        Ok(chunk) => chunk,
                        Err(_) => break,
                    };
                    chunk.samples.clear();
                    chunk.frame_end_positions.clear();
                    chunk.frame_loop_remaining.clear();
                    let start_position = atomic_load_f64(&producer_position_state_thread);
                    let start_loop_remaining =
                        producer_loop_remaining_state_thread.load(Ordering::Acquire);
                    let mut eof = false;
                    for _ in 0..PCM_PREFETCH_CHUNK_FRAMES {
                        let mut frame_samples = 0usize;
                        let mut frame_eof = false;
                        for _ in 0..channel_count {
                            match source.next() {
                                Some(sample) => {
                                    chunk.samples.push(sample);
                                    frame_samples += 1;
                                }
                                None => {
                                    frame_eof = true;
                                    break;
                                }
                            }
                        }

                        if frame_samples == 0 {
                            eof = true;
                            break;
                        }

                        if frame_samples < channel_count {
                            chunk
                                .samples
                                .extend(std::iter::repeat(0.0).take(channel_count - frame_samples));
                            frame_eof = true;
                        }

                        // Store the exact media position after this output frame.
                        // A chunk can cross a loop boundary, so start/end interpolation
                        // would otherwise move the playhead through the wrong timeline.
                        chunk
                            .frame_end_positions
                            .push(atomic_load_f64(&producer_position_state_thread).to_bits());
                        chunk
                            .frame_loop_remaining
                            .push(producer_loop_remaining_state_thread.load(Ordering::Acquire));

                        if frame_eof {
                            eof = true;
                            break;
                        }
                    }
                    let end_position = atomic_load_f64(&producer_position_state_thread);
                    chunk.start_position_bits = start_position.to_bits();
                    chunk.end_position_bits = end_position.to_bits();
                    chunk.start_loop_remaining = start_loop_remaining;
                    chunk.eof = eof;

                    if !chunk.samples.is_empty() {
                        if let Err(error) = ready_tx.send(chunk) {
                            drop(error.0);
                            break;
                        }
                    } else {
                        drop(chunk);
                    }

                    if eof {
                        producer_finished_thread.store(true, Ordering::Release);
                        // Close the producer side so an empty/short source can
                        // report EOF immediately after any final queued chunk.
                        drop(ready_tx);
                        // Keep the guard receiver alive until the Cue drops its
                        // free-buffer sender. Returned buffers and queued tail
                        // chunks are then released on this producer thread.
                        while let Ok(returned) = free_rx.recv() {
                            drop(returned);
                        }
                        while let Ok(queued) = ready_rx_guard.try_recv() {
                            drop(queued);
                        }
                        return;
                    }
                }

                producer_finished_thread.store(true, Ordering::Release);
                while let Ok(queued) = ready_rx_guard.try_recv() {
                    drop(queued);
                }
            });
        if let Err(error) = producer {
            error!("Could not start PCM prefetch thread: {error}");
            return None;
        }

        // Prime at least one chunk before attaching the source to the output
        // mixer. While waiting, continue pumping DirectSound's queued output so
        // a long PlayBatch preparation cannot starve already-playing Cue.
        let prime_started = Instant::now();
        let first_chunk = loop {
            match ready_rx.try_recv() {
                Ok(chunk) => break chunk,
                Err(_) if producer_finished.load(Ordering::Acquire) && ready_rx.is_empty() => {
                    warn!("PCM prefetch source ended before producing its first chunk");
                    return None;
                }
                Err(_) => {
                    pump_during_wait();
                    if prime_started.elapsed() >= Duration::from_secs(5) {
                        warn!(
                            "PCM prefetch could not prime its first audio chunk within 5 seconds"
                        );
                        return None;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            }
        };

        Some(Self {
            channels,
            sample_rate,
            total_duration,
            ready_rx,
            free_tx,
            current_chunk: Some(first_chunk),
            current_index: 0,
            producer_finished,
            render_position_state,
            underrun_count,
            playback_elapsed_frames,
            render_loop_remaining_state,
            underrun_frame_count,
            underrun_silence_samples_remaining: 0,
            underrun_active: false,
            loop_diagnostics_enabled,
            render_loop_iterations,
            render_loop_boundary_frame,
            render_loop_boundary_position_bits,
            render_loop_boundary_endpoint_frame,
            endpoint_frame_clock,
            render_loop_fingerprint,
            render_loop_fingerprint_iteration,
            loop_pcm_capture,
            loop_capture_pre_samples: [0.0; PCM_LOOP_CAPTURE_PRE_FRAMES
                * PCM_LOOP_CAPTURE_MAX_CHANNELS],
            loop_capture_pre_frames: 0,
            loop_capture_pre_write_frame: 0,
            loop_capture_active: false,
            loop_capture_iteration: 0,
            loop_capture_post_samples_written: 0,
            last_render_media_position: None,
            loop_fingerprint_active: false,
            loop_fingerprint_samples_remaining: 0,
            loop_fingerprint_hash: PCM_FNV1A64_OFFSET_BASIS,
            loop_fingerprint_iteration: 0,
            consumer_finished: false,
        })
    }
}

impl Iterator for PcmPrefetchSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.consumer_finished {
            return None;
        }

        // If an underrun began at the start of an interleaved frame, finish
        // that entire frame with silence before trying the ready queue again.
        // Otherwise a chunk arriving between channel reads could shift channel
        // alignment (for example, silence on L followed by new PCM on R).
        if self.underrun_silence_samples_remaining > 0 {
            self.invalidate_loop_pcm_capture(PCM_LOOP_CAPTURE_STATUS_UNDERRUN);
            self.underrun_silence_samples_remaining -= 1;
            return Some(0.0);
        }

        if self.current_chunk.is_none() {
            match self.ready_rx.try_recv() {
                Ok(chunk) => {
                    self.current_chunk = Some(chunk);
                    self.current_index = 0;
                    self.underrun_active = false;
                }
                Err(_)
                    if self.producer_finished.load(Ordering::Acquire)
                        && self.ready_rx.is_empty() =>
                {
                    self.consumer_finished = true;
                    return None;
                }
                Err(_) => {
                    // Count every silent source frame, including one-frame
                    // scheduling gaps. The previous 32-frame threshold hid
                    // short gaps that are still enough to desynchronize Cue.
                    self.invalidate_loop_pcm_capture(PCM_LOOP_CAPTURE_STATUS_UNDERRUN);
                    self.underrun_frame_count.fetch_add(1, Ordering::Relaxed);
                    // Do not publish a fingerprint that includes synthetic silence.
                    self.loop_fingerprint_active = false;
                    if !self.underrun_active {
                        self.underrun_count.fetch_add(1, Ordering::Relaxed);
                        self.underrun_active = true;
                    }
                    let channel_count = self.channels.get() as usize;
                    self.underrun_silence_samples_remaining = channel_count.saturating_sub(1);
                    self.playback_elapsed_frames.fetch_add(1, Ordering::Relaxed);
                    return Some(0.0);
                }
            }
        }

        if self.current_index % self.channels.get() as usize == 0 {
            let frame_index = self.current_index / self.channels.get() as usize;
            let (position, loop_remaining) = match self.current_chunk.as_ref() {
                Some(chunk) => (
                    prefetch_chunk_frame_start_position(chunk, frame_index),
                    prefetch_chunk_frame_start_loop_remaining(chunk, frame_index),
                ),
                None => return Some(0.0),
            };
            if self.loop_diagnostics_enabled {
                if let Some(previous_position) = self.last_render_media_position {
                    // Playback speed is always positive. A backwards media-time
                    // transition therefore marks a Loop wrap, not a producer event.
                    if position + 1.0e-9 < previous_position {
                        let source_elapsed_frame =
                            self.playback_elapsed_frames.load(Ordering::Relaxed);
                        self.render_loop_boundary_frame
                            .store(source_elapsed_frame, Ordering::Release);
                        self.render_loop_boundary_position_bits
                            .store(position.to_bits(), Ordering::Release);
                        let source_media_frame = if position.is_finite() && position > 0.0 {
                            (position * f64::from(self.sample_rate.get())).round() as u64
                        } else {
                            0
                        };
                        let endpoint_render_frame = self
                            .endpoint_frame_clock
                            .as_ref()
                            .map(|clock| clock.load(Ordering::Relaxed))
                            .unwrap_or(u64::MAX);
                        self.render_loop_boundary_endpoint_frame
                            .store(endpoint_render_frame, Ordering::Release);
                        // Publish the iteration only after its boundary metadata
                        // is visible to the worker thread.
                        let iteration =
                            self.render_loop_iterations.fetch_add(1, Ordering::AcqRel) + 1;
                        self.begin_loop_pcm_capture(
                            iteration,
                            source_elapsed_frame,
                            source_media_frame,
                            endpoint_render_frame,
                        );
                        // Fingerprint the first 128 rendered source frames after
                        // the wrap to distinguish timing drift from decoder/seek
                        // returning different PCM content.
                        self.loop_fingerprint_active = true;
                        self.loop_fingerprint_samples_remaining =
                            PCM_LOOP_FINGERPRINT_FRAMES * self.channels.get() as usize;
                        self.loop_fingerprint_hash = PCM_FNV1A64_OFFSET_BASIS;
                        self.loop_fingerprint_iteration = iteration;
                    }
                }
                self.last_render_media_position = Some(position);
            }
            atomic_store_f64(&self.render_position_state, position);
            self.render_loop_remaining_state
                .store(loop_remaining, Ordering::Release);
        }

        let sample = self
            .current_chunk
            .as_ref()
            .and_then(|chunk| chunk.samples.get(self.current_index))
            .copied();
        let Some(sample) = sample else {
            self.consumer_finished = true;
            return None;
        };
        if self.loop_fingerprint_active {
            self.loop_fingerprint_hash ^= sample.to_bits() as u64;
            self.loop_fingerprint_hash = self.loop_fingerprint_hash.wrapping_mul(PCM_FNV1A64_PRIME);
            self.loop_fingerprint_samples_remaining =
                self.loop_fingerprint_samples_remaining.saturating_sub(1);
            if self.loop_fingerprint_samples_remaining == 0 {
                self.render_loop_fingerprint
                    .store(self.loop_fingerprint_hash, Ordering::Release);
                self.render_loop_fingerprint_iteration
                    .store(self.loop_fingerprint_iteration, Ordering::Release);
                self.loop_fingerprint_active = false;
            }
        }
        self.record_loop_pcm_capture_sample(sample);
        self.current_index += 1;
        if self.current_index % self.channels.get() as usize == 0 {
            self.playback_elapsed_frames.fetch_add(1, Ordering::Relaxed);
        }

        let chunk_finished = self
            .current_chunk
            .as_ref()
            .is_some_and(|chunk| self.current_index >= chunk.samples.len());
        if chunk_finished {
            if let Some(mut completed) = self.current_chunk.take() {
                let eof = completed.eof;
                // Publish the exact chunk-end state once. Inside a chunk, the
                // next frame's start metadata already carries the previous
                // frame's end position and remaining-loop count.
                atomic_store_f64(
                    &self.render_position_state,
                    f64::from_bits(completed.end_position_bits),
                );
                self.render_loop_remaining_state.store(
                    completed
                        .frame_loop_remaining
                        .last()
                        .copied()
                        .unwrap_or(completed.start_loop_remaining),
                    Ordering::Release,
                );
                completed.samples.clear();
                completed.frame_end_positions.clear();
                completed.frame_loop_remaining.clear();
                self.current_index = 0;
                if eof {
                    self.consumer_finished = true;
                }
                if let Err(error) = self.free_tx.try_send(completed) {
                    retire_pcm_buffer(error.into_inner());
                }
            }
        }

        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

impl Source for PcmPrefetchSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> std::num::NonZeroU16 {
        self.channels
    }

    fn sample_rate(&self) -> std::num::NonZeroU32 {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.total_duration
    }
}

impl Drop for PcmPrefetchSource {
    fn drop(&mut self) {
        if let Some(chunk) = self.current_chunk.take() {
            retire_pcm_buffer(chunk);
        }
        // The producer's guard receiver owns any still-queued chunks and drains
        // them on its own thread after free_tx disconnects.
    }
}

const CHANNEL_ROUTER_SETTINGS_REFRESH_FRAMES: usize = 64;

/// Per-Cue routing snapshot used by the audio source. The vectors are allocated
/// when the source is prepared and are only updated in place, so rendering does
/// not need to clone route data or wait for a UI/control-thread write lock.
struct CachedChannelRoute {
    active: bool,
    master_enabled: bool,
    output_channel: usize,
    explicit_destinations: bool,
    output_channels: Vec<usize>,
    gain_linear: f32,
}

#[derive(Clone, Copy)]
struct CachedOutputChannelSetting {
    active: bool,
    gain_linear: f32,
}

#[allow(dead_code)]
struct ChannelRouterRetiredBuffers {
    input_frame: Vec<f32>,
    output_frame: Vec<f32>,
    cached_routes: Vec<CachedChannelRoute>,
    cached_output_settings: Vec<CachedOutputChannelSetting>,
    cached_output_gains: Vec<f32>,
    routes: Arc<RwLock<Vec<ChannelRoute>>>,
    output_settings: SharedOutputSettings,
    output_gains: Arc<RwLock<Vec<f32>>>,
}

static CHANNEL_ROUTER_REAPER: OnceLock<Option<Sender<ChannelRouterRetiredBuffers>>> =
    OnceLock::new();

fn channel_router_reaper() -> Option<&'static Sender<ChannelRouterRetiredBuffers>> {
    CHANNEL_ROUTER_REAPER
        .get_or_init(|| {
            let (sender, receiver) = bounded::<ChannelRouterRetiredBuffers>(256);
            match thread::Builder::new()
                .name("tasq-channel-router-reaper".to_string())
                .spawn(move || {
                    while let Ok(buffers) = receiver.recv() {
                        drop(buffers);
                    }
                }) {
                Ok(_) => Some(sender),
                Err(error) => {
                    error!("Could not start ChannelRouter buffer reaper thread: {error}");
                    None
                }
            }
        })
        .as_ref()
}

struct ChannelRouter<S> {
    source: S,
    input_channels: usize,
    output_channels: usize,
    output_index: usize,
    input_frame: Vec<f32>,
    output_frame: Vec<f32>,
    routes: Arc<RwLock<Vec<ChannelRoute>>>,
    output_settings: SharedOutputSettings,
    output_gains: Arc<RwLock<Vec<f32>>>,
    cached_routes: Vec<CachedChannelRoute>,
    cached_output_settings: Vec<CachedOutputChannelSetting>,
    cached_output_gains: Vec<f32>,
    frames_until_settings_refresh: usize,
    profile: Option<Arc<AudioStreamHealth>>,
    profile_frame_counter: u64,
}

impl<S> ChannelRouter<S>
where
    S: Source,
{
    #[cfg(test)]
    fn new(
        source: S,
        routes: Arc<RwLock<Vec<ChannelRoute>>>,
        output_settings: SharedOutputSettings,
        output_gains: Arc<RwLock<Vec<f32>>>,
        output_channels: usize,
    ) -> Self {
        Self::new_with_profile(
            source,
            routes,
            output_settings,
            output_gains,
            output_channels,
            None,
        )
    }

    fn new_with_profile(
        source: S,
        routes: Arc<RwLock<Vec<ChannelRoute>>>,
        output_settings: SharedOutputSettings,
        output_gains: Arc<RwLock<Vec<f32>>>,
        output_channels: usize,
        profile: Option<Arc<AudioStreamHealth>>,
    ) -> Self {
        let _ = channel_router_reaper();
        let input_channels = source.channels().get().max(1) as usize;
        let output_channels = output_channels.max(1);
        {
            let mut gains = output_gains.write();
            gains.resize(output_channels, 0.0);
            gains.truncate(output_channels);
        }

        // Keep the route table aligned with the decoder's real channel count.
        // This also provides a deterministic default map for legacy projects
        // whose per-track route list was empty or shorter than the audio stream.
        {
            let mut route_list = routes.write();
            route_list.truncate(input_channels);
            while route_list.len() < input_channels {
                let source_index = route_list.len();
                let output_channel = source_index.min(output_channels - 1);
                route_list.push(ChannelRoute {
                    active: true,
                    master_enabled: true,
                    output_channel,
                    output_channels: vec![output_channel],
                    gain_db: 0.0,
                });
            }
            for route in route_list.iter_mut() {
                route.output_channel = route.output_channel.min(output_channels - 1);
                route
                    .output_channels
                    .retain(|index| *index < output_channels);
                route.output_channels.sort_unstable();
                route.output_channels.dedup();
                if route.output_channels.is_empty() && route.active {
                    route.output_channels.push(route.output_channel);
                }
                if let Some(primary) = route.output_channels.first().copied() {
                    route.output_channel = primary;
                }
            }
        }

        let output_state = output_settings
            .read()
            .iter()
            .map(|setting| (setting.active, setting.gain_db))
            .collect::<Vec<_>>();
        let cue_gains = output_gains.read().clone();
        info!(
            "ChannelRouter configured: {} source channels -> {} hardware outputs; output_settings(active,gain_db)={:?}; cue_output_gains_db={:?}",
            input_channels, output_channels, output_state, cue_gains
        );

        // Build all render-side snapshots before the source is attached to the
        // native output stream. Each route's destination capacity is bounded by
        // the endpoint's output-channel count.
        let cached_routes = {
            let route_list = routes.read();
            let mut cached = Vec::with_capacity(input_channels);
            for source_index in 0..input_channels {
                let fallback_output = source_index.min(output_channels - 1);
                let route = route_list.get(source_index);
                let active = route.map(|route| route.active).unwrap_or(true);
                let master_enabled = route.map(|route| route.master_enabled).unwrap_or(true);
                let output_channel = route
                    .map(|route| route.output_channel.min(output_channels - 1))
                    .unwrap_or(fallback_output);
                let route_destinations = route.map(|route| route.output_channels.as_slice());
                let explicit_destinations = route_destinations
                    .map(|destinations| !destinations.is_empty())
                    .unwrap_or(true);
                let mut destinations = Vec::with_capacity(output_channels);
                if let Some(route_destinations) = route_destinations {
                    for &destination in route_destinations {
                        if destination < output_channels && !destinations.contains(&destination) {
                            destinations.push(destination);
                        }
                    }
                } else {
                    destinations.push(fallback_output);
                }
                cached.push(CachedChannelRoute {
                    active,
                    master_enabled,
                    output_channel,
                    explicit_destinations,
                    output_channels: destinations,
                    gain_linear: db_to_linear(route.map(|route| route.gain_db).unwrap_or(0.0)),
                });
            }
            cached
        };

        let mut cached_output_settings = vec![
            CachedOutputChannelSetting {
                active: true,
                gain_linear: 1.0,
            };
            output_channels
        ];
        if let Some(settings) = output_settings.try_read() {
            for (cached, setting) in cached_output_settings.iter_mut().zip(settings.iter()) {
                cached.active = setting.active;
                cached.gain_linear = db_to_linear(setting.gain_db);
            }
        }

        let mut cached_output_gains = vec![1.0; output_channels];
        if let Some(gains) = output_gains.try_read() {
            for (cached, gain) in cached_output_gains.iter_mut().zip(gains.iter()) {
                *cached = db_to_linear(*gain);
            }
        }

        if let Some(profile) = profile.as_ref() {
            profile
                .channel_router_active_instances
                .fetch_add(1, Ordering::Relaxed);
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
            output_gains,
            cached_routes,
            cached_output_settings,
            cached_output_gains,
            frames_until_settings_refresh: 0,
            profile,
            profile_frame_counter: 0,
        }
    }

    /// Refresh mutable routing and gain state without ever waiting in the audio
    /// path. If the control thread currently owns a write lock, retain the last
    /// complete snapshot and try again at the next refresh interval.
    fn refresh_live_settings(&mut self) {
        if let Some(routes) = self.routes.try_read() {
            for source_index in 0..self.input_channels {
                let cached = &mut self.cached_routes[source_index];
                if let Some(route) = routes.get(source_index) {
                    cached.active = route.active;
                    cached.master_enabled = route.master_enabled;
                    cached.output_channel = route.output_channel.min(self.output_channels - 1);
                    cached.explicit_destinations = !route.output_channels.is_empty();
                    cached.gain_linear = db_to_linear(route.gain_db);
                    cached.output_channels.clear();
                    if cached.explicit_destinations {
                        for &destination in &route.output_channels {
                            if destination < self.output_channels
                                && !cached.output_channels.contains(&destination)
                            {
                                // Capacity was reserved to the endpoint's output
                                // count during construction; no callback allocation.
                                cached.output_channels.push(destination);
                            }
                        }
                    }
                } else {
                    let fallback_output = source_index.min(self.output_channels - 1);
                    cached.active = true;
                    cached.master_enabled = true;
                    cached.output_channel = fallback_output;
                    cached.explicit_destinations = true;
                    cached.gain_linear = 1.0;
                    cached.output_channels.clear();
                    cached.output_channels.push(fallback_output);
                }
            }
        }

        if let Some(settings) = self.output_settings.try_read() {
            for (index, cached) in self.cached_output_settings.iter_mut().enumerate() {
                if let Some(setting) = settings.get(index) {
                    cached.active = setting.active;
                    cached.gain_linear = db_to_linear(setting.gain_db);
                } else {
                    *cached = CachedOutputChannelSetting {
                        active: true,
                        gain_linear: 1.0,
                    };
                }
            }
        }

        if let Some(gains) = self.output_gains.try_read() {
            for (index, cached) in self.cached_output_gains.iter_mut().enumerate() {
                *cached = db_to_linear(gains.get(index).copied().unwrap_or(0.0));
            }
        }
    }

    fn mix_to_output(
        output_frame: &mut [f32],
        output_settings: &[CachedOutputChannelSetting],
        output_gains: &[f32],
        output_index: usize,
        sample: f32,
    ) {
        if output_index >= output_frame.len() {
            return;
        }
        let setting = output_settings.get(output_index);
        if setting.map(|setting| !setting.active).unwrap_or(false) {
            return;
        }
        let device_gain = setting.map(|setting| setting.gain_linear).unwrap_or(1.0);
        let cue_gain = output_gains.get(output_index).copied().unwrap_or(1.0);
        output_frame[output_index] += sample * device_gain * cue_gain;
    }
}

impl<S> Drop for ChannelRouter<S> {
    fn drop(&mut self) {
        if let Some(profile) = self.profile.as_ref() {
            profile
                .channel_router_active_instances
                .fetch_sub(1, Ordering::Relaxed);
        }

        let retired = ChannelRouterRetiredBuffers {
            input_frame: std::mem::take(&mut self.input_frame),
            output_frame: std::mem::take(&mut self.output_frame),
            cached_routes: std::mem::take(&mut self.cached_routes),
            cached_output_settings: std::mem::take(&mut self.cached_output_settings),
            cached_output_gains: std::mem::take(&mut self.cached_output_gains),
            routes: self.routes.clone(),
            output_settings: self.output_settings.clone(),
            output_gains: self.output_gains.clone(),
        };
        if let Some(reaper) = channel_router_reaper() {
            if let Err(error) = reaper.try_send(retired) {
                // Queue overflow is exceptionally unlikely. Avoid freeing these
                // buffers on the native callback thread if it does occur.
                std::mem::forget(error.into_inner());
            }
        } else {
            std::mem::forget(retired);
        }
    }
}

impl<S> Iterator for ChannelRouter<S>
where
    S: Source,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.output_index >= self.output_channels {
            let profile_sample = if self.profile.is_some() {
                let sample = self.profile_frame_counter % CHANNEL_ROUTER_PROFILE_STRIDE == 0;
                self.profile_frame_counter = self.profile_frame_counter.wrapping_add(1);
                sample
            } else {
                false
            };

            let source_started = profile_sample.then(Instant::now);
            for sample in &mut self.input_frame {
                *sample = self.source.next()?;
            }
            let source_elapsed_ns = source_started
                .map(|started| started.elapsed().as_nanos().min(u64::MAX as u128) as u64)
                .unwrap_or(0);

            let route_started = profile_sample.then(Instant::now);
            self.output_frame.fill(0.0);

            if self.frames_until_settings_refresh == 0 {
                self.refresh_live_settings();
                self.frames_until_settings_refresh =
                    CHANNEL_ROUTER_SETTINGS_REFRESH_FRAMES.saturating_sub(1);
            } else {
                self.frames_until_settings_refresh -= 1;
            }

            for (source_index, sample) in self.input_frame.iter().copied().enumerate() {
                let route = &self.cached_routes[source_index];
                if !route.active || !route.master_enabled {
                    continue;
                }

                let source_gain = route.gain_linear;
                if !route.explicit_destinations {
                    Self::mix_to_output(
                        &mut self.output_frame,
                        &self.cached_output_settings,
                        &self.cached_output_gains,
                        route.output_channel,
                        sample * source_gain,
                    );
                } else {
                    for &output_index in &route.output_channels {
                        Self::mix_to_output(
                            &mut self.output_frame,
                            &self.cached_output_settings,
                            &self.cached_output_gains,
                            output_index,
                            sample * source_gain,
                        );
                    }
                }
            }

            if profile_sample {
                if let Some(profile) = self.profile.as_ref() {
                    let route_elapsed_ns = route_started
                        .map(|started| started.elapsed().as_nanos().min(u64::MAX as u128) as u64)
                        .unwrap_or(0);
                    let estimated_frames = CHANNEL_ROUTER_PROFILE_STRIDE;
                    profile
                        .channel_router_profile_samples
                        .fetch_add(1, Ordering::Relaxed);
                    profile
                        .channel_router_profiled_frames
                        .fetch_add(estimated_frames, Ordering::Relaxed);
                    profile.channel_router_source_total_ns.fetch_add(
                        source_elapsed_ns.saturating_mul(estimated_frames),
                        Ordering::Relaxed,
                    );
                    profile
                        .channel_router_source_max_ns
                        .fetch_max(source_elapsed_ns, Ordering::Relaxed);
                    profile.channel_router_route_total_ns.fetch_add(
                        route_elapsed_ns.saturating_mul(estimated_frames),
                        Ordering::Relaxed,
                    );
                    profile
                        .channel_router_route_max_ns
                        .fetch_max(route_elapsed_ns, Ordering::Relaxed);
                }
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
    S: Source,
{
    fn current_span_len(&self) -> Option<usize> {
        // A cue keeps one fixed channel count and sample rate for its lifetime.
        // Returning the decoder's transformed span length here lets Rodio's
        // UniformSourceIterator split it at its internal 32,768-sample limit.
        // That limit is not necessarily divisible by our output channel count
        // (for 6 channels: 32,768 % 6 == 2), so the next converter span can
        // restart in the middle of an interleaved frame and rotate channels.
        // `None` means the format remains stable until this cue ends.
        None
    }

    fn channels(&self) -> std::num::NonZeroU16 {
        std::num::NonZeroU16::new(self.output_channels.min(u16::MAX as usize) as u16)
            .expect("ChannelRouter always has at least one output channel")
    }

    fn sample_rate(&self) -> std::num::NonZeroU32 {
        self.source.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.source.total_duration()
    }
}

/// Holds a prepared Cue at silence until the endpoint's shared sample-frame
/// clock reaches the batch target. The gate is armed only after every Cue in
/// the batch has been decoded and attached to the same mixer.
struct SampleScheduledSource<S> {
    source: S,
    frame_clock: Arc<AtomicU64>,
    start_frame_gate: Arc<AtomicU64>,
    start_frame_observed: Option<Arc<AtomicU64>>,
    started: bool,
}

impl<S> SampleScheduledSource<S> {
    fn new(
        source: S,
        frame_clock: Arc<AtomicU64>,
        start_frame_gate: Arc<AtomicU64>,
        start_frame_observed: Option<Arc<AtomicU64>>,
    ) -> Self {
        Self {
            source,
            frame_clock,
            start_frame_gate,
            start_frame_observed,
            started: false,
        }
    }
}

impl<S> Iterator for SampleScheduledSource<S>
where
    S: Source,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.started {
            let target_frame = self.start_frame_gate.load(Ordering::Acquire);
            let current_frame = self.frame_clock.load(Ordering::Relaxed);
            if target_frame == u64::MAX || current_frame < target_frame {
                return Some(0.0);
            }
            if let Some(observed) = &self.start_frame_observed {
                observed.store(current_frame, Ordering::Release);
            }
            self.started = true;
        }
        self.source.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let (_, upper) = self.source.size_hint();
        (0, upper)
    }
}

impl<S> Source for SampleScheduledSource<S>
where
    S: Source,
{
    fn current_span_len(&self) -> Option<usize> {
        // The leading silence has a dynamic length determined by the shared
        // output clock, so expose one continuous span to Rodio.
        None
    }

    fn channels(&self) -> std::num::NonZeroU16 {
        self.source.channels()
    }

    fn sample_rate(&self) -> std::num::NonZeroU32 {
        self.source.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.source.total_duration()
    }

    fn try_seek(
        &mut self,
        position: Duration,
    ) -> std::result::Result<(), rodio::source::SeekError> {
        self.source.try_seek(position)
    }
}

/// Rodio's Player normally attaches its queue to the mixer while the queue is
/// still empty (and therefore reports the Empty source's mono channel count).
/// The queue can then expose a 6-channel source while its initial 512-sample
/// span is still in flight. 512 is not divisible by 6, so UniformSourceIterator
/// can resume in the middle of an interleaved frame and rotate every channel.
///
/// Queue the first source before attaching this wrapper to the mixer, and hide
/// the queue's artificial span boundaries. A Cue owns one fixed output layout
/// for its lifetime, so one continuous span is correct and keeps frame alignment.
struct FrameAlignedPlayerQueue {
    inner: rodio::queue::SourcesQueueOutput,
}

impl Iterator for FrameAlignedPlayerQueue {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl Source for FrameAlignedPlayerQueue {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> std::num::NonZeroU16 {
        self.inner.channels()
    }

    fn sample_rate(&self) -> std::num::NonZeroU32 {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    fn try_seek(
        &mut self,
        position: Duration,
    ) -> std::result::Result<(), rodio::source::SeekError> {
        self.inner.try_seek(position)
    }
}

/// Sample-accurate de-click ramp used by explicit Stop commands.
/// It fades the final, already-routed output signal to zero over a short
/// raised-cosine window, then ends the source. The control thread never
/// changes Sink volume in small timer steps.
struct StopFadeSource<S> {
    inner: S,
    control: StopFadeControl,
    channels: usize,
    fade_frames: u64,
    fade_frame: u64,
    channel_index: usize,
    current_gain: f32,
    fading: bool,
    completion_pending: bool,
}

impl<S> StopFadeSource<S>
where
    S: Source<Item = f32>,
{
    fn new(inner: S, control: StopFadeControl) -> Self {
        let channels = inner.channels().get().max(1) as usize;
        let sample_rate = inner.sample_rate().get().max(1);
        let fade_frames =
            ((sample_rate as f64 * STOP_DECLICK_FADE_MS / 1_000.0).round() as u64).max(1);

        Self {
            inner,
            control,
            channels,
            fade_frames,
            fade_frame: 0,
            channel_index: 0,
            current_gain: 1.0,
            fading: false,
            completion_pending: false,
        }
    }
}

impl<S> Iterator for StopFadeSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        // Check completion only once per complete interleaved frame. Besides
        // preventing a completion signal from truncating a multichannel frame,
        // this avoids one Acquire atomic load for every output channel/sample.
        if self.channel_index == 0 {
            if self.control.is_completed() {
                return None;
            }

            // This branch is reached only after the last sample of the fade has
            // already been returned to the mixer.
            if self.completion_pending {
                self.control.completed.store(true, Ordering::Release);
                return None;
            }

            // Start the ramp only at an output-frame boundary so every channel
            // in an interleaved frame receives exactly the same gain.
            if !self.fading && self.control.requested.load(Ordering::Acquire) {
                self.fading = true;
                self.fade_frame = 0;
            }

            self.current_gain = if self.fading {
                if self.fade_frame + 1 >= self.fade_frames {
                    0.0
                } else {
                    let denominator = self.fade_frames.saturating_sub(1).max(1) as f32;
                    let progress = (self.fade_frame as f32 / denominator).clamp(0.0, 1.0);
                    0.5 * (1.0 + (std::f32::consts::PI * progress).cos())
                }
            } else {
                1.0
            };
        }

        let sample = match self.inner.next() {
            Some(sample) => sample,
            None => {
                if self.fading || self.control.requested.load(Ordering::Acquire) {
                    self.control.completed.store(true, Ordering::Release);
                }
                return None;
            }
        };

        let output = sample * self.current_gain;
        self.channel_index += 1;

        if self.channel_index >= self.channels {
            self.channel_index = 0;
            if self.fading {
                self.fade_frame = self.fade_frame.saturating_add(1);
                if self.fade_frame >= self.fade_frames {
                    self.completion_pending = true;
                }
            }
        }

        Some(output)
    }
}

impl<S> Source for StopFadeSource<S>
where
    S: Source<Item = f32>,
{
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> std::num::NonZeroU16 {
        self.inner.channels()
    }

    fn sample_rate(&self) -> std::num::NonZeroU32 {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

fn open_sink_at(
    mixer: &rodio::mixer::Mixer,
    file_path: &PathBuf,
    offset_secs: f64,
    initial_vol: f32,
    loop_enabled: bool,
    loop_start_secs: f64,
    loop_end_secs: f64,
    loops_remaining: Arc<AtomicI32>,
    loop_iterations: Arc<AtomicU64>,
    playback_elapsed_frames: Arc<AtomicU64>,
    render_loop_remaining_state: Arc<AtomicI32>,
    stop_fade: StopFadeControl,
    output_channels: usize,
    output_sample_rate: u32,
    routes: Arc<RwLock<Vec<ChannelRoute>>>,
    output_settings: SharedOutputSettings,
    output_gains: Arc<RwLock<Vec<f32>>>,
    channel_router_profile: Option<Arc<AudioStreamHealth>>,
    automation_enabled: bool,
    automation_points: Vec<[f64; 2]>,
    automation_updates: Receiver<VolumeAutomationUpdate>,
    retired_automation_points_tx: Sender<Vec<[f64; 2]>>,
    time_pitch: Arc<RwLock<TimePitchSettings>>,
    position_state: Arc<AtomicU64>,
    prefetch_underrun_count: Arc<AtomicU64>,
    prefetch_underrun_frame_count: Arc<AtomicU64>,
    render_loop_iterations: Arc<AtomicU64>,
    render_loop_boundary_frame: Arc<AtomicU64>,
    render_loop_boundary_position_bits: Arc<AtomicU64>,
    render_loop_boundary_endpoint_frame: Arc<AtomicU64>,
    loop_pcm_capture: Arc<LoopPcmCapture>,
    render_loop_fingerprint: Arc<AtomicU64>,
    render_loop_fingerprint_iteration: Arc<AtomicU64>,
    pump_during_prefetch: &mut dyn FnMut(),
    start_frame_gate: Option<Arc<AtomicU64>>,
    sample_frame_clock: Option<Arc<AtomicU64>>,
    start_frame_observed: Option<Arc<AtomicU64>>,
) -> Option<(rodio::Player, u32)> {
    let file = std::fs::File::open(file_path).ok()?;
    let decoder = rodio::Decoder::try_from(file).ok()?;
    let source_channels = decoder.channels().get();
    let source_sample_rate = decoder.sample_rate().get();
    debug!(
        "CUE SOURCE FORMAT: file='{}', source={} channels @ {} Hz, endpoint={} channels @ {} Hz; TaSQ converts the source to endpoint format before sample scheduling",
        file_path.display(),
        source_channels,
        source_sample_rate,
        output_channels,
        output_sample_rate
    );
    let loop_count_at_start = loops_remaining.load(Ordering::Acquire);
    let producer_initial_remaining = if loop_enabled && loop_end_secs > loop_start_secs {
        loop_count_at_start
    } else {
        0
    };
    let producer_loop_remaining_state = Arc::new(AtomicI32::new(producer_initial_remaining));
    let loop_source_active = loop_enabled
        && loop_end_secs > loop_start_secs
        && (loop_count_at_start == -1 || loop_count_at_start > 1);
    let (source, loop_timeline): (Box<dyn Source<Item = f32> + Send>, Option<LoopTimeline>) =
        if loop_source_active {
            let looping = LoopingSource::new(
                decoder,
                offset_secs.max(0.0),
                loop_start_secs,
                loop_end_secs,
                true,
                loops_remaining,
                loop_iterations,
                file_path.display().to_string(),
            );
            let timeline = looping.timeline.clone();
            (Box::new(looping), timeline)
        } else {
            (
                Box::new(decoder.skip_duration(Duration::from_secs_f64(offset_secs.max(0.0)))),
                None,
            )
        };
    let producer_position_state = Arc::new(AtomicU64::new(offset_secs.max(0.0).to_bits()));
    let transformed = SpeedPitchSource::new_with_loop(
        source,
        offset_secs.max(0.0),
        time_pitch,
        producer_position_state.clone(),
        loop_timeline,
        producer_loop_remaining_state.clone(),
    );
    let prefetched = PcmPrefetchSource::new(
        transformed,
        producer_position_state,
        position_state.clone(),
        prefetch_underrun_count,
        playback_elapsed_frames,
        producer_loop_remaining_state,
        render_loop_remaining_state,
        prefetch_underrun_frame_count,
        loop_source_active,
        render_loop_iterations,
        render_loop_boundary_frame,
        render_loop_boundary_position_bits,
        render_loop_boundary_endpoint_frame,
        sample_frame_clock.clone(),
        render_loop_fingerprint,
        render_loop_fingerprint_iteration,
        loop_pcm_capture,
        pump_during_prefetch,
    )?;
    let automated = VolumeAutomationSource::new(
        prefetched,
        automation_enabled,
        automation_points,
        automation_updates,
        retired_automation_points_tx,
        position_state,
    );
    let routed = ChannelRouter::new_with_profile(
        automated,
        routes,
        output_settings,
        output_gains,
        output_channels,
        channel_router_profile,
    );
    // Convert each Cue to the endpoint's exact channel/sample-rate layout
    // before applying the shared frame gate. This is important: the scheduler
    // must count output frames, not decoder-rate frames.
    let target_channels =
        std::num::NonZeroU16::new(output_channels.max(1).min(u16::MAX as usize) as u16)?;
    let target_sample_rate = std::num::NonZeroU32::new(output_sample_rate.max(1))?;
    let endpoint_source =
        rodio::source::UniformSourceIterator::new(routed, target_channels, target_sample_rate);
    // Build the Player queue without connecting it yet. Its first queued
    // source already has the final multichannel layout when Rodio creates the
    // mixer adapter, preventing an initial mono/512-sample bootstrap.
    let (player, queue_source) = rodio::Player::new();
    player.set_volume(initial_vol);
    if let Some((start_frame_gate, sample_frame_clock)) = start_frame_gate.zip(sample_frame_clock) {
        let scheduled = SampleScheduledSource::new(
            endpoint_source,
            sample_frame_clock,
            start_frame_gate,
            start_frame_observed,
        );
        player.append(StopFadeSource::new(scheduled, stop_fade.clone()));
    } else {
        // Normal single-Cue playback stays on the immediate, zero-extra-check path.
        player.append(StopFadeSource::new(endpoint_source, stop_fade.clone()));
    }
    mixer.add(FrameAlignedPlayerQueue {
        inner: queue_source,
    });
    player.play();
    Some((player, source_sample_rate))
}

/// Continuous multichannel test tone. `current_span_len = None` is deliberate:
/// Rodio's UniformSourceIterator must not split a 6-channel source at an arbitrary
/// sample count such as 32,768, because that count is not frame-aligned for 6ch.
struct RawOutputTestTone {
    output_channels: usize,
    selected_channel: usize,
    sample_rate: u32,
    sample_index: u64,
    duration_frames: u64,
    fade_frames: u64,
    generation: u64,
    current_generation: Arc<AtomicU64>,
    stop_frame: Option<u64>,
}

impl Iterator for RawOutputTestTone {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        let channels = self.output_channels.max(1) as u64;
        let channel = (self.sample_index % channels) as usize;
        let frame = self.sample_index / channels;
        if frame >= self.duration_frames {
            return None;
        }
        self.sample_index = self.sample_index.wrapping_add(1);

        if self.current_generation.load(Ordering::Relaxed) != self.generation
            && self.stop_frame.is_none()
        {
            self.stop_frame = Some(frame);
        }

        let fade_frames = self.fade_frames.max(1);
        let envelope = if let Some(stop_frame) = self.stop_frame {
            let elapsed = frame.saturating_sub(stop_frame);
            if elapsed >= fade_frames {
                return None;
            }
            1.0 - elapsed as f32 / fade_frames as f32
        } else {
            let fade_in = (frame as f32 / fade_frames as f32).min(1.0);
            let remaining = self.duration_frames.saturating_sub(frame);
            let fade_out = (remaining as f32 / fade_frames as f32).min(1.0);
            fade_in.min(fade_out)
        };

        if channel != self.selected_channel {
            return Some(0.0);
        }

        let phase = std::f32::consts::TAU * 440.0 * frame as f32 / self.sample_rate.max(1) as f32;
        Some(phase.sin() * 0.12 * envelope)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // Report an unknown upper bound deliberately. Rodio's uniformizer can
        // split finite sources at a fixed sample-count span; keeping the span
        // open avoids rotating a multichannel test signal at that boundary.
        (0, None)
    }
}

impl Source for RawOutputTestTone {
    fn current_span_len(&self) -> Option<usize> {
        // Keep the source's channel phase stable across Rodio's former
        // 32,768-sample internal span boundary.
        None
    }

    fn channels(&self) -> std::num::NonZeroU16 {
        std::num::NonZeroU16::new(self.output_channels as u16)
            .expect("test tone requires at least one output channel")
    }

    fn sample_rate(&self) -> std::num::NonZeroU32 {
        std::num::NonZeroU32::new(self.sample_rate).expect("test tone requires a valid sample rate")
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

const DIRECTSOUND_DEFAULT_DEVICE: &str = "System Default (DirectSound)";
#[cfg(target_os = "windows")]
const DIRECTSOUND_DEFAULT_QUEUE_FRAMES: usize = 4_096;

#[allow(dead_code)]
enum OutputStreamHandle {
    Cpal(cpal::Stream),
    // Use SDL's queue API instead of its callback API. SDL2 0.38's callback
    // trampoline can panic on Windows when its callback userdata is observed
    // before initialization; a queue avoids that callback thread entirely.
    #[cfg(target_os = "windows")]
    DirectSound(DirectSoundQueue),
}

enum AudioOutputContext {
    Cpal(Host),
    #[cfg(target_os = "windows")]
    DirectSound {
        // Keep SDL and its audio subsystem alive for every opened SDL device.
        _sdl: sdl2::Sdl,
        audio: sdl2::AudioSubsystem,
    },
}

#[cfg(target_os = "windows")]
struct DirectSoundQueue {
    device_id: sdl2::sys::SDL_AudioDeviceID,
    channels: usize,
    sample_rate: u32,
    target_queue_frames: usize,
}

#[cfg(target_os = "windows")]
impl DirectSoundQueue {
    fn open(
        device_name: Option<&str>,
        channels: u8,
        sample_rate: u32,
        target_queue_frames: usize,
    ) -> std::result::Result<Self, String> {
        let device_name = device_name
            .map(CString::new)
            .transpose()
            .map_err(|error| format!("Invalid DirectSound device name: {error}"))?;
        let device_ptr = device_name
            .as_ref()
            .map_or(ptr::null(), |name| name.as_ptr());

        let desired_format = sdl2::audio::AudioFormat::f32_sys() as sdl2::sys::SDL_AudioFormat;
        let desired = sdl2::sys::SDL_AudioSpec {
            freq: sample_rate as i32,
            format: desired_format,
            channels,
            silence: 0,
            samples: target_queue_frames.clamp(256, u16::MAX as usize) as u16,
            padding: 0,
            size: 0,
            callback: None,
            userdata: ptr::null_mut(),
        };
        let mut obtained = MaybeUninit::<sdl2::sys::SDL_AudioSpec>::uninit();

        // Use SDL's raw API here instead of sdl2::AudioQueue::open_queue.
        // SDL2 0.38 converts the returned SDL_AudioSpec with an internal
        // Option::unwrap(); some DirectSound drivers report a format that the
        // wrapper does not recognize, which panics the entire worker thread.
        // The raw API lets us validate the returned format and report an error.
        let device_id = unsafe {
            sdl2::sys::SDL_OpenAudioDevice(device_ptr, 0, &desired, obtained.as_mut_ptr(), 0)
        };
        if device_id == 0 {
            return Err(Self::last_sdl_error());
        }

        let obtained = unsafe { obtained.assume_init() };
        if obtained.format != desired_format {
            let actual_format = obtained.format;
            let sdl_error = Self::last_sdl_error();
            unsafe {
                sdl2::sys::SDL_CloseAudioDevice(device_id);
            }
            return Err(format!(
                "DirectSound returned unsupported sample format 0x{:04X} (requested float32); {}",
                actual_format, sdl_error
            ));
        }
        if obtained.channels == 0 || obtained.freq <= 0 {
            unsafe {
                sdl2::sys::SDL_CloseAudioDevice(device_id);
            }
            return Err(format!(
                "DirectSound returned invalid stream format: {} channels @ {} Hz",
                obtained.channels, obtained.freq
            ));
        }

        Ok(Self {
            device_id,
            channels: obtained.channels as usize,
            sample_rate: obtained.freq as u32,
            target_queue_frames: target_queue_frames.max(256),
        })
    }

    fn last_sdl_error() -> String {
        let error = unsafe { sdl2::sys::SDL_GetError() };
        if error.is_null() {
            "SDL2 did not provide an error description".to_string()
        } else {
            unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned()
        }
    }

    fn queue_audio(&self, samples: &[f32]) -> std::result::Result<(), String> {
        if samples.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::size_of_val(samples);
        if bytes > u32::MAX as usize {
            return Err("DirectSound queue buffer exceeds SDL2's size limit".to_string());
        }
        let result = unsafe {
            sdl2::sys::SDL_QueueAudio(self.device_id, samples.as_ptr().cast(), bytes as u32)
        };
        if result == 0 {
            Ok(())
        } else {
            Err(Self::last_sdl_error())
        }
    }

    fn queued_bytes(&self) -> u32 {
        unsafe { sdl2::sys::SDL_GetQueuedAudioSize(self.device_id) }
    }

    fn resume(&self) {
        unsafe {
            sdl2::sys::SDL_PauseAudioDevice(self.device_id, 0);
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for DirectSoundQueue {
    fn drop(&mut self) {
        debug!(
            "Releasing DirectSound queue device {}: pause, clear queued PCM, then close",
            self.device_id
        );
        unsafe {
            sdl2::sys::SDL_PauseAudioDevice(self.device_id, 1);
            sdl2::sys::SDL_ClearQueuedAudio(self.device_id);
            sdl2::sys::SDL_CloseAudioDevice(self.device_id);
        }
        debug!("Released DirectSound queue device {}", self.device_id);
    }
}

#[cfg(target_os = "windows")]
struct DirectSoundInitInfo {
    output_devices: Vec<String>,
    current_device: String,
    output_channels: usize,
    sample_rate: u32,
    output_device_channels: HashMap<String, usize>,
    output_device_channel_layouts: HashMap<String, Vec<usize>>,
}

#[derive(Default)]
struct AudioStreamHealth {
    /// Incremented once per native output callback, not per sample.
    callback_count: AtomicU64,
    /// Cumulative callback execution time, updated without logging in the callback.
    callback_total_ns: AtomicU64,
    /// Longest observed callback execution time.
    callback_max_ns: AtomicU64,
    /// Number of callbacks whose processing time reached/exceeded their buffer period.
    callback_over_budget_count: AtomicU64,
    callback_last_frames: AtomicU64,
    /// Low-overhead sampled timing for per-Cue source generation inside ChannelRouter.
    channel_router_active_instances: AtomicU64,
    channel_router_profile_samples: AtomicU64,
    channel_router_profiled_frames: AtomicU64,
    channel_router_source_total_ns: AtomicU64,
    channel_router_source_max_ns: AtomicU64,
    channel_router_route_total_ns: AtomicU64,
    channel_router_route_max_ns: AtomicU64,
    /// Sampled wall-time for rendering a complete endpoint frame from Rodio's mixer.
    mixer_output_profile_samples: AtomicU64,
    mixer_output_profiled_frames: AtomicU64,
    mixer_output_total_ns: AtomicU64,
    mixer_output_max_ns: AtomicU64,
    /// Stop requests published by the audio control thread and acknowledged after
    /// a subsequent output callback has rendered its complete buffer.
    stop_request_generation: AtomicU64,
    stop_callback_ack_generation: AtomicU64,
    /// Set by CPAL's stream-error callback; the control thread consumes it.
    stream_error: AtomicBool,
}

/// Samples the wall-time needed to pull one complete interleaved endpoint frame
/// from Rodio's mixer. The timer is armed once every MIXER_OUTPUT_PROFILE_STRIDE
/// frames and is accumulated outside the native callback's logging path.
struct MixerOutputProfileSource<S> {
    source: S,
    health: Arc<AudioStreamHealth>,
    channels: usize,
    channel_index: usize,
    frame_counter: u64,
    frame_is_sampled: bool,
    frame_started_at: Option<Instant>,
}

impl<S> MixerOutputProfileSource<S> {
    fn new(source: S, health: Arc<AudioStreamHealth>, channels: usize) -> Self {
        Self {
            source,
            health,
            channels: channels.max(1),
            channel_index: 0,
            frame_counter: 0,
            frame_is_sampled: false,
            frame_started_at: None,
        }
    }
}

impl<S> Iterator for MixerOutputProfileSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.channel_index == 0 {
            self.frame_is_sampled = self.frame_counter % MIXER_OUTPUT_PROFILE_STRIDE == 0;
            self.frame_started_at = self.frame_is_sampled.then(Instant::now);
        }

        let Some(sample) = self.source.next() else {
            self.frame_started_at = None;
            self.frame_is_sampled = false;
            return None;
        };

        self.channel_index += 1;
        if self.channel_index >= self.channels {
            self.channel_index = 0;

            if self.frame_is_sampled {
                if let Some(started_at) = self.frame_started_at.take() {
                    let elapsed_ns = started_at.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                    self.health
                        .mixer_output_profile_samples
                        .fetch_add(1, Ordering::Relaxed);
                    self.health
                        .mixer_output_profiled_frames
                        .fetch_add(MIXER_OUTPUT_PROFILE_STRIDE, Ordering::Relaxed);
                    self.health.mixer_output_total_ns.fetch_add(
                        elapsed_ns.saturating_mul(MIXER_OUTPUT_PROFILE_STRIDE),
                        Ordering::Relaxed,
                    );
                    self.health
                        .mixer_output_max_ns
                        .fetch_max(elapsed_ns, Ordering::Relaxed);
                }
            }

            self.frame_is_sampled = false;
            self.frame_counter = self.frame_counter.wrapping_add(1);
        }

        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.source.size_hint()
    }
}

impl<S> Source for MixerOutputProfileSource<S>
where
    S: Source<Item = f32>,
{
    fn current_span_len(&self) -> Option<usize> {
        self.source.current_span_len()
    }

    fn channels(&self) -> std::num::NonZeroU16 {
        self.source.channels()
    }

    fn sample_rate(&self) -> std::num::NonZeroU32 {
        self.source.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.source.total_duration()
    }

    fn try_seek(
        &mut self,
        position: Duration,
    ) -> std::result::Result<(), rodio::source::SeekError> {
        self.channel_index = 0;
        self.frame_counter = 0;
        self.frame_is_sampled = false;
        self.frame_started_at = None;
        self.source.try_seek(position)
    }
}

struct PendingStopCallbackAck {
    generation: u64,
    request_id: Uuid,
    cue_id: Option<Uuid>,
    requested_at: Instant,
}

struct DeviceOutput {
    // Keeping the native stream/device alive is required for audio playback.
    _stream: OutputStreamHandle,
    mixer: rodio::mixer::Mixer,
    channels: usize,
    sample_rate: u32,
    performance: AudioPerformanceSettings,
    settings: SharedOutputSettings,
    /// Present for CPAL streams. DirectSound currently uses SDL's queued-output
    /// path and has no CPAL callback to monitor.
    stream_health: Option<Arc<AudioStreamHealth>>,
    /// Present for CPAL endpoints. The final-mix gate pauses every Cue on the
    /// endpoint together instead of toggling individual Rodio Sinks in sequence.
    transport_paused: Option<Arc<AtomicBool>>,
    /// Monotonic count of output sample frames rendered by this endpoint.
    /// Cue sources in the same Worker read this clock to share an exact start frame.
    sample_frame_clock: Arc<AtomicU64>,
    last_callback_count: u64,
    last_callback_progress_at: Instant,
    pending_stop_callback_acks: VecDeque<PendingStopCallbackAck>,
    last_stop_callback_ack_generation: u64,
    last_callback_metrics_at: Instant,
    last_callback_metrics_count: u64,
    last_callback_total_ns: u64,
    last_callback_over_budget_count: u64,
    #[cfg(target_os = "windows")]
    directsound_source:
        Option<SampleFrameClockSource<OutputPeakMeterSource<rodio::mixer::MixerSource>>>,
}

#[inline]
fn player_output_is_globally_paused(
    streams: &HashMap<String, DeviceOutput>,
    player: &ActivePlayer,
) -> bool {
    streams
        .get(&player.output_device)
        .and_then(|output| output.transport_paused.as_ref())
        .is_some_and(|paused| paused.load(Ordering::Acquire))
}

#[inline]
fn player_is_paused(streams: &HashMap<String, DeviceOutput>, player: &ActivePlayer) -> bool {
    player.paused_at.is_some()
        || player.sink.is_paused()
        || player_output_is_globally_paused(streams, player)
}

const ASIO_STREAM_CALLBACK_STALL_TIMEOUT: Duration = Duration::from_millis(750);
const ASIO_STREAM_INITIAL_CALLBACK_TIMEOUT: Duration = Duration::from_secs(3);

fn shared_output_settings(
    settings_by_device: &OutputSettingsByDevice,
    device_name: &str,
    channels: usize,
) -> SharedOutputSettings {
    let settings = {
        let mut all_settings = settings_by_device.write();
        all_settings
            .entry(device_name.to_string())
            .or_insert_with(|| {
                Arc::new(RwLock::new(vec![
                    OutputChannelSetting::default();
                    channels.max(1)
                ]))
            })
            .clone()
    };
    {
        let mut values = settings.write();
        values.resize(channels.max(1), OutputChannelSetting::default());
        values.truncate(channels.max(1));
    }
    settings
}

fn output_config_candidates(
    backend: AudioBackend,
    device: &Device,
    preferred: cpal::SupportedStreamConfig,
    requested_sample_rate: Option<u32>,
) -> Vec<cpal::SupportedStreamConfig> {
    let mut candidates = vec![preferred.clone()];

    if backend == AudioBackend::Asio {
        // ASIO's default configuration reflects the driver's active clock and
        // format. Re-enumerating the driver for alternatives adds latency and
        // risks selecting a configuration that the driver itself did not offer.
        return candidates;
    }

    if backend == AudioBackend::Wasapi {
        // WASAPI Shared should follow the Windows endpoint mix format in Auto,
        // or the explicitly selected supported rate. Do not probe unrelated
        // sample rates: the shared audio engine performs system-side conversion.
        let rate = requested_sample_rate.unwrap_or(preferred.sample_rate());
        let channels = preferred.channels();
        if let Ok(ranges) = device.supported_output_configs() {
            for range in ranges {
                if range.channels() == channels && range.contains_rate(rate) {
                    let candidate = range.with_sample_rate(rate);
                    if !candidates.contains(&candidate) {
                        candidates.push(candidate);
                    }
                }
            }
        }
        candidates.sort_by_key(|config| {
            std::cmp::Reverse((
                (config.sample_format() == preferred.sample_format()) as u8,
                (config.sample_format() == cpal::SampleFormat::F32) as u8,
            ))
        });
        candidates.truncate(8);
        return candidates;
    }

    if let Some(rate) = requested_sample_rate {
        if let Ok(ranges) = device.supported_output_configs() {
            for range in ranges {
                if range.channels() == preferred.channels() && range.contains_rate(rate) {
                    let candidate = range.with_sample_rate(rate);
                    if !candidates.contains(&candidate) {
                        candidates.push(candidate);
                    }
                }
            }
        }
        candidates.retain(|config| config.sample_rate() == rate);
        candidates.sort_by_key(|config| {
            std::cmp::Reverse((
                config.channels(),
                (config.sample_format() == cpal::SampleFormat::F32) as u8,
            ))
        });
        candidates.truncate(48);
        return candidates;
    }

    if let Ok(ranges) = device.supported_output_configs() {
        for range in ranges {
            let mut rates = vec![
                48_000,
                44_100,
                range.max_sample_rate(),
                range.min_sample_rate(),
            ];
            rates.dedup();
            for rate in rates {
                if range.contains_rate(rate) {
                    let candidate = range.with_sample_rate(rate);
                    if !candidates.contains(&candidate) {
                        candidates.push(candidate);
                    }
                }
            }
        }
    }

    if let Ok(default_config) = device.default_output_config() {
        if !candidates.contains(&default_config) {
            candidates.push(default_config);
        }
    }

    let rate_rank = |rate: u32| match rate {
        48_000 => 6_u8,
        44_100 => 5,
        96_000 => 4,
        88_200 => 3,
        192_000 => 2,
        176_400 => 1,
        _ => 0,
    };
    let format_rank = |format: cpal::SampleFormat| match format {
        cpal::SampleFormat::F32 => 3_u8,
        cpal::SampleFormat::F64 => 2,
        _ => 1,
    };

    candidates.sort_by_key(|config| {
        std::cmp::Reverse((
            config.channels(),
            rate_rank(config.sample_rate()),
            format_rank(config.sample_format()),
        ))
    });
    candidates.truncate(48);
    candidates
}

fn build_device_output(
    backend: AudioBackend,
    device: &Device,
    preferred_config: cpal::SupportedStreamConfig,
    device_name: &str,
    settings_by_device: &OutputSettingsByDevice,
    output_peak_meter: &Arc<AtomicU32>,
    performance: AudioPerformanceSettings,
) -> std::result::Result<DeviceOutput, String> {
    let performance = AudioPerformanceSettings::for_backend(backend, performance);
    let preferred_config_for_order = preferred_config.clone();
    let candidate_sample_rate = if backend == AudioBackend::Asio {
        Some(preferred_config.sample_rate())
    } else {
        performance.sample_rate
    };
    let candidates =
        output_config_candidates(backend, device, preferred_config, candidate_sample_rate);
    let device_id = device
        .id()
        .map(|id| id.to_string())
        .unwrap_or_else(|_| "unavailable".to_string());
    debug!(
        "Opening output endpoint '{}' [{}] with {} candidate configuration(s)",
        device_name,
        device_id,
        candidates.len()
    );
    let mut candidates = candidates;
    if let Some(index) = candidates
        .iter()
        .position(|candidate| candidate == &preferred_config_for_order)
    {
        let preferred = candidates.remove(index);
        candidates.insert(0, preferred);
    }
    let mut failures = Vec::new();

    for config in candidates {
        let channels = (config.channels() as usize).max(1);
        let sample_rate = config.sample_rate();
        let sample_format = config.sample_format();

        let requested_buffer = performance
            .buffer_size_frames
            .map_or(cpal::BufferSize::Default, cpal::BufferSize::Fixed);
        debug!(
            "Trying output '{}' with {} channels @ {} Hz ({:?}), buffer={:?}",
            device_name, channels, sample_rate, sample_format, requested_buffer
        );

        let mixer_channels = std::num::NonZeroU16::new(channels as u16)
            .ok_or_else(|| format!("Output endpoint '{}' reported zero channels", device_name))?;
        let mixer_sample_rate = std::num::NonZeroU32::new(sample_rate).ok_or_else(|| {
            format!(
                "Output endpoint '{}' reported an invalid sample rate",
                device_name
            )
        })?;
        let (mixer, source) = rodio::mixer::mixer(mixer_channels, mixer_sample_rate);
        let stream_health = Arc::new(AudioStreamHealth::default());
        let profiled_mixer_source =
            MixerOutputProfileSource::new(source, stream_health.clone(), channels);
        let transport_paused = Arc::new(AtomicBool::new(false));
        let gated_source =
            OutputTransportGateSource::new(profiled_mixer_source, transport_paused.clone());
        let metered_source = OutputPeakMeterSource::new(gated_source, output_peak_meter.clone());
        let sample_frame_clock = Arc::new(AtomicU64::new(0));
        // Keep the endpoint frame clock outside the gate: it continues to count
        // silent hardware frames while the Cue mixer itself is frozen.
        let clocked_source =
            SampleFrameClockSource::new(metered_source, sample_frame_clock.clone(), channels);
        let stream_config = cpal::StreamConfig {
            channels: config.channels(),
            sample_rate: config.sample_rate(),
            buffer_size: requested_buffer,
        };
        let stream = match build_metered_cpal_stream(
            device,
            stream_config,
            sample_format,
            clocked_source,
            device_name,
            stream_health.clone(),
        ) {
            Ok(stream) => stream,
            Err(open_error) => {
                let device_busy = open_error.contains("DeviceBusy")
                    || open_error.contains("AUDCLNT_E_DEVICE_IN_USE")
                    || open_error.contains("-2004287478")
                    || open_error.contains("0x8889000A");
                if device_busy {
                    let message = format!(
                        "Output endpoint '{}' is busy and cannot be opened by {}. Another backend or application may already hold the device: {}",
                        device_name,
                        backend.short_label(),
                        open_error
                    );
                    warn!("{message}");
                    return Err(message);
                }
                failures.push(format!(
                    "{}ch @ {}Hz {:?}: {}",
                    channels, sample_rate, sample_format, open_error
                ));
                continue;
            }
        };
        if let Err(play_error) = stream.play() {
            failures.push(format!(
                "{}ch @ {}Hz {:?}: could not start stream: {}",
                channels, sample_rate, sample_format, play_error
            ));
            continue;
        }

        info!(
            "Audio output stream opened successfully: '{}' ({} channels @ {} Hz, {:?}, requested sample_rate={:?}, requested buffer={:?}; final-mix peak metering enabled)",
            device_name,
            channels,
            sample_rate,
            sample_format,
            performance.sample_rate,
            requested_buffer
        );
        let settings = shared_output_settings(settings_by_device, device_name, channels);
        return Ok(DeviceOutput {
            _stream: OutputStreamHandle::Cpal(stream),
            mixer,
            channels,
            sample_rate,
            performance,
            settings,
            stream_health: Some(stream_health),
            transport_paused: Some(transport_paused),
            sample_frame_clock,
            last_callback_count: 0,
            last_callback_progress_at: Instant::now(),
            pending_stop_callback_acks: VecDeque::new(),
            last_stop_callback_ack_generation: 0,
            last_callback_metrics_at: Instant::now(),
            last_callback_metrics_count: 0,
            last_callback_total_ns: 0,
            last_callback_over_budget_count: 0,
            #[cfg(target_os = "windows")]
            directsound_source: None,
        });
    }

    let message = format!(
        "Could not open audio output '{}'. Tried {} supported configurations. Details: {}",
        device_name,
        failures.len(),
        failures.join(" | ")
    );
    warn!("{}", message);
    Err(message)
}

fn build_metered_cpal_stream<S>(
    device: &Device,
    stream_config: cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    source: S,
    _device_name: &str,
    stream_health: Arc<AudioStreamHealth>,
) -> std::result::Result<cpal::Stream, String>
where
    S: Iterator<Item = f32> + Send + 'static,
{
    let callback_channels = stream_config.channels.max(1) as usize;
    let callback_sample_rate = stream_config.sample_rate.max(1) as u64;
    macro_rules! build_output_stream {
        ($sample_type:ty) => {{
            let mut source = source;
            let callback_health = stream_health.clone();
            let error_health = stream_health.clone();
            device
                .build_output_stream::<$sample_type, _, _>(
                    stream_config.clone(),
                    move |data: &mut [$sample_type], _| {
                        let stop_generation_at_start = callback_health
                            .stop_request_generation
                            .load(Ordering::Acquire);
                        let callback_started = Instant::now();
                        callback_health
                            .callback_count
                            .fetch_add(1, Ordering::Relaxed);
                        data.iter_mut().for_each(|sample| {
                            *sample = source
                                .next()
                                .map(Sample::from_sample)
                                .unwrap_or(<$sample_type>::EQUILIBRIUM);
                        });
                        let elapsed_ns =
                            callback_started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                        callback_health
                            .callback_total_ns
                            .fetch_add(elapsed_ns, Ordering::Relaxed);
                        callback_health
                            .callback_max_ns
                            .fetch_max(elapsed_ns, Ordering::Relaxed);
                        let frames = data.len() / callback_channels;
                        callback_health
                            .callback_last_frames
                            .store(frames as u64, Ordering::Relaxed);
                        let budget_ns =
                            (frames as u64).saturating_mul(1_000_000_000) / callback_sample_rate;
                        if budget_ns > 0 && elapsed_ns >= budget_ns {
                            callback_health
                                .callback_over_budget_count
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        // Acknowledgement means a complete output buffer was rendered
                        // after the control thread published the stop request. It does
                        // not mean the physical DAC buffer has drained yet.
                        let stop_generation_after_render = callback_health
                            .stop_request_generation
                            .load(Ordering::Acquire);
                        if stop_generation_after_render == stop_generation_at_start {
                            callback_health
                                .stop_callback_ack_generation
                                .store(stop_generation_after_render, Ordering::Release);
                        }
                    },
                    move |_error| {
                        // Do not format or write logs from the native stream callback.
                        // The audio control loop observes this lock-free flag and
                        // reports the failure through the supervised Worker process.
                        error_health.stream_error.store(true, Ordering::Release);
                    },
                    None,
                )
                .map_err(|error| format!("{error} [{error:?}]"))
        }};
    }

    match sample_format {
        cpal::SampleFormat::F32 => build_output_stream!(f32),
        cpal::SampleFormat::F64 => build_output_stream!(f64),
        cpal::SampleFormat::I8 => build_output_stream!(i8),
        cpal::SampleFormat::I16 => build_output_stream!(i16),
        cpal::SampleFormat::I24 => build_output_stream!(cpal::I24),
        cpal::SampleFormat::I32 => build_output_stream!(i32),
        cpal::SampleFormat::I64 => build_output_stream!(i64),
        cpal::SampleFormat::U8 => build_output_stream!(u8),
        cpal::SampleFormat::U16 => build_output_stream!(u16),
        cpal::SampleFormat::U24 => build_output_stream!(cpal::U24),
        cpal::SampleFormat::U32 => build_output_stream!(u32),
        cpal::SampleFormat::U64 => build_output_stream!(u64),
        unsupported => Err(format!("Unsupported output sample format: {unsupported:?}")),
    }
}

#[cfg(target_os = "windows")]
fn directsound_device_spec(audio: &sdl2::AudioSubsystem, device_name: &str) -> (usize, u32) {
    if device_name == DIRECTSOUND_DEFAULT_DEVICE {
        return (2, 48_000);
    }

    let count = audio.num_audio_playback_devices().unwrap_or(0);
    for index in 0..count {
        let Ok(name) = audio.audio_playback_device_name(index) else {
            continue;
        };
        if name != device_name {
            continue;
        }

        let mut spec = sdl2::sys::SDL_AudioSpec {
            freq: 0,
            format: 0,
            channels: 0,
            silence: 0,
            samples: 0,
            padding: 0,
            size: 0,
            callback: None,
            userdata: ptr::null_mut(),
        };
        let result = unsafe { sdl2::sys::SDL_GetAudioDeviceSpec(index as i32, 0, &mut spec) };
        if result == 0 && spec.channels > 0 && spec.freq > 0 {
            return (spec.channels as usize, (spec.freq as u32).max(8_000));
        }
        debug!(
            "SDL2 could not report a usable DirectSound format for '{}': {}",
            device_name,
            DirectSoundQueue::last_sdl_error()
        );
        break;
    }

    (2, 48_000)
}

#[cfg(target_os = "windows")]
fn list_directsound_devices(audio: &sdl2::AudioSubsystem) -> Vec<String> {
    let mut devices = vec![DIRECTSOUND_DEFAULT_DEVICE.to_string()];
    for index in 0..audio.num_audio_playback_devices().unwrap_or(0) {
        if let Ok(name) = audio.audio_playback_device_name(index) {
            if !name.trim().is_empty() && !devices.iter().any(|known| known == &name) {
                devices.push(name);
            }
        }
    }
    devices
}

#[cfg(target_os = "windows")]
fn build_directsound_device_output(
    audio: &sdl2::AudioSubsystem,
    device_name: &str,
    settings_by_device: &OutputSettingsByDevice,
    output_peak_meter: &Arc<AtomicU32>,
    performance: AudioPerformanceSettings,
) -> Option<DeviceOutput> {
    let device_spec = directsound_device_spec(audio, device_name);
    build_directsound_device_output_with_spec(
        device_name,
        settings_by_device,
        output_peak_meter,
        performance,
        device_spec,
    )
}

#[cfg(target_os = "windows")]
fn build_directsound_device_output_with_spec(
    device_name: &str,
    settings_by_device: &OutputSettingsByDevice,
    output_peak_meter: &Arc<AtomicU32>,
    performance: AudioPerformanceSettings,
    device_spec: (usize, u32),
) -> Option<DeviceOutput> {
    use std::num::{NonZeroU16, NonZeroU32};

    let (channels, preferred_rate) = device_spec;
    let channels_u8 = u8::try_from(channels).ok().filter(|value| *value > 0)?;
    let selected_device = (device_name != DIRECTSOUND_DEFAULT_DEVICE).then_some(device_name);

    let queue_target_frames = performance
        .buffer_size_frames
        .map(|frames| frames as usize)
        .unwrap_or(DIRECTSOUND_DEFAULT_QUEUE_FRAMES)
        .clamp(512, 32_768);
    let mut rates = Vec::new();
    let requested_rates: Vec<u32> = if let Some(rate) = performance.sample_rate {
        vec![rate]
    } else {
        vec![preferred_rate, 48_000, 44_100, 96_000]
    };
    for rate in requested_rates {
        if !rates.contains(&rate) {
            rates.push(rate);
        }
    }

    let mut last_error = None;
    for sample_rate in rates {
        let queue = match DirectSoundQueue::open(
            selected_device,
            channels_u8,
            sample_rate,
            queue_target_frames,
        ) {
            Ok(queue) => queue,
            Err(error) => {
                debug!(
                    "DirectSound queue configuration rejected for '{}': {} channels @ {} Hz: {}",
                    device_name, channels, sample_rate, error
                );
                last_error = Some(error);
                continue;
            }
        };

        let actual_channels = queue.channels.max(1);
        let actual_sample_rate = queue.sample_rate.max(1);
        let Some(mixer_channels) = NonZeroU16::new(actual_channels as u16) else {
            last_error = Some(format!(
                "DirectSound returned an invalid channel count: {actual_channels}"
            ));
            continue;
        };
        let Some(mixer_rate) = NonZeroU32::new(actual_sample_rate) else {
            last_error = Some("DirectSound returned an invalid sample rate".to_string());
            continue;
        };
        let (mixer, source) = rodio::mixer::mixer(mixer_channels, mixer_rate);
        let metered_source = OutputPeakMeterSource::new(source, output_peak_meter.clone());
        let sample_frame_clock = Arc::new(AtomicU64::new(0));
        let source = SampleFrameClockSource::new(
            metered_source,
            sample_frame_clock.clone(),
            actual_channels,
        );

        // Prime the queue with silence before resuming playback.
        let initial_silence = vec![0.0_f32; actual_channels * queue.target_queue_frames];
        if let Err(error) = queue.queue_audio(&initial_silence) {
            debug!(
                "Could not prime DirectSound queue for '{}': {}",
                device_name, error
            );
            last_error = Some(error);
            continue;
        }

        queue.resume();
        info!(
            "DirectSound output opened: '{}' ({} channels @ {} Hz; queue={} frames / {:.1} ms; requested performance={:?}; float32; SDL driver: directsound)",
            device_name,
            actual_channels,
            actual_sample_rate,
            queue.target_queue_frames,
            queue.target_queue_frames as f64 * 1_000.0 / actual_sample_rate as f64,
            performance
        );

        let settings = shared_output_settings(settings_by_device, device_name, actual_channels);
        return Some(DeviceOutput {
            _stream: OutputStreamHandle::DirectSound(queue),
            mixer,
            channels: actual_channels,
            sample_rate: actual_sample_rate,
            performance,
            settings,
            stream_health: None,
            transport_paused: None,
            sample_frame_clock,
            last_callback_count: 0,
            last_callback_progress_at: Instant::now(),
            pending_stop_callback_acks: VecDeque::new(),
            last_stop_callback_ack_generation: 0,
            last_callback_metrics_at: Instant::now(),
            last_callback_metrics_count: 0,
            last_callback_total_ns: 0,
            last_callback_over_budget_count: 0,
            directsound_source: Some(source),
        });
    }

    warn!(
        "Could not open DirectSound output '{}': {}",
        device_name,
        last_error.unwrap_or_else(|| "no supported DirectSound configuration".to_string())
    );
    None
}

fn open_device_output(
    backend: AudioBackend,
    device_name: &str,
    context: &AudioOutputContext,
    settings_by_device: &OutputSettingsByDevice,
    output_peak_meter: &Arc<AtomicU32>,
    performance: AudioPerformanceSettings,
) -> Option<DeviceOutput> {
    let performance = AudioPerformanceSettings::for_backend(backend, performance);
    match context {
        AudioOutputContext::Cpal(host) => {
            let devices = host.output_devices().ok()?;
            let device = devices
                .into_iter()
                .find(|device| device.to_string() == device_name)?;
            let config = preferred_output_config(backend, &device, performance.sample_rate)
                .or_else(|| {
                    device.default_output_config().ok().filter(|config| {
                        performance
                            .sample_rate
                            .map_or(true, |rate| config.sample_rate() == rate)
                    })
                })?;
            build_device_output(
                backend,
                &device,
                config,
                device_name,
                settings_by_device,
                output_peak_meter,
                performance,
            )
            .ok()
        }
        #[cfg(target_os = "windows")]
        AudioOutputContext::DirectSound { audio, .. } => build_directsound_device_output(
            audio,
            device_name,
            settings_by_device,
            output_peak_meter,
            performance,
        ),
    }
}
#[cfg(target_os = "windows")]
fn pump_directsound_queues(streams: &mut HashMap<String, DeviceOutput>) {
    for (device_name, output) in streams.iter_mut() {
        let Some(source) = output.directsound_source.as_mut() else {
            continue;
        };
        let OutputStreamHandle::DirectSound(queue) = &output._stream else {
            continue;
        };

        let channels = output.channels.max(1);
        let bytes_per_frame = channels * std::mem::size_of::<f32>();
        let queued_frames = queue.queued_bytes() as usize / bytes_per_frame;
        let low_watermark = (queue.target_queue_frames / 2).max(1);
        if queued_frames >= low_watermark {
            continue;
        }

        let frames_to_queue = queue.target_queue_frames.saturating_sub(queued_frames);
        if frames_to_queue == 0 {
            continue;
        }

        let sample_count = frames_to_queue * channels;
        let mut samples = Vec::with_capacity(sample_count);
        for _ in 0..sample_count {
            samples.push(source.next().unwrap_or(0.0));
        }

        if let Err(error) = queue.queue_audio(&samples) {
            warn!(
                "Could not refill DirectSound output queue for '{}': {}",
                device_name, error
            );
        }
    }
}

fn ensure_device_output(
    backend: AudioBackend,
    streams: &mut HashMap<String, DeviceOutput>,
    device_name: &str,
    context: &AudioOutputContext,
    settings_by_device: &OutputSettingsByDevice,
    output_peak_meter: &Arc<AtomicU32>,
    performance: AudioPerformanceSettings,
) -> Option<(rodio::mixer::Mixer, usize, u32, SharedOutputSettings)> {
    let needs_reopen = streams
        .get(device_name)
        .is_some_and(|output| output.performance != performance);
    if needs_reopen {
        warn!(
            "Reopening output '{}' because its performance settings changed: {:?} -> {:?}",
            device_name,
            streams.get(device_name).map(|output| output.performance),
            performance
        );
        streams.remove(device_name);
    }
    if !streams.contains_key(device_name) {
        let output = open_device_output(
            backend,
            device_name,
            context,
            settings_by_device,
            output_peak_meter,
            performance,
        )?;
        streams.insert(device_name.to_string(), output);
    }
    let output = streams.get_mut(device_name)?;
    output.settings = shared_output_settings(settings_by_device, device_name, output.channels);
    Some((
        output.mixer.clone(),
        output.channels,
        output.sample_rate,
        output.settings.clone(),
    ))
}

fn audio_worker(
    backend: AudioBackend,
    initial_device_id: String,
    initial_device_name: String,
    cmd_rx: Receiver<AudioCommand>,
    pos_tx: Sender<PositionUpdate>,
    output_settings_by_device: OutputSettingsByDevice,
    output_peak_meter: Arc<AtomicU32>,
    performance: AudioPerformanceSettings,
    ready_tx: Sender<Result<(usize, u32), String>>,
) {
    let host = match host_for_backend(backend) {
        Ok(host) => host,
        Err(error) => {
            error!(
                "Could not initialise {} audio host in worker: {}",
                backend.label(),
                error
            );
            let _ = ready_tx.send(Err(format!(
                "Could not initialise {} audio host: {}",
                backend.label(),
                error
            )));
            return;
        }
    };

    // CPAL's WASAPI Device wraps a native COM endpoint. Do not move a Device
    // handle created on the GUI thread into the audio thread: some vendor
    // drivers reject stream creation from a different COM apartment. Resolve
    // the endpoint again on this thread, then open it here.
    let initial_device_by_id = initial_device_id
        .parse::<cpal::DeviceId>()
        .ok()
        .and_then(|device_id| host.device_by_id(&device_id));
    let initial_device = if initial_device_by_id.is_some() {
        initial_device_by_id
    } else {
        match host.output_devices() {
            Ok(devices) => devices
                .into_iter()
                .find(|device| device.to_string() == initial_device_name),
            Err(error) => {
                error!(
                    "Could not enumerate {} output devices on the audio worker thread: {}",
                    backend.label(),
                    error
                );
                let _ = ready_tx.send(Err(format!(
                    "Could not enumerate {} output devices on the audio worker thread: {}",
                    backend.label(),
                    error
                )));
                return;
            }
        }
    };
    let Some(initial_device) = initial_device else {
        let message = format!(
            "Selected {} output device '{}' disappeared before the audio worker opened it",
            backend.label(),
            initial_device_name
        );
        error!("{}", message);
        let _ = ready_tx.send(Err(message));
        return;
    };

    let device_id = initial_device
        .id()
        .map(|id| id.to_string())
        .unwrap_or_else(|_| "unavailable".to_string());
    debug!(
        "Resolved initial {} endpoint on audio worker thread: '{}' (requested ID: '{}', resolved ID: '{}')",
        backend.label(),
        initial_device_name,
        initial_device_id,
        device_id
    );

    let initial_config = preferred_output_config(backend, &initial_device, performance.sample_rate)
        .or_else(|| {
            initial_device
                .default_output_config()
                .ok()
                .filter(|config| {
                    performance
                        .sample_rate
                        .map_or(true, |rate| config.sample_rate() == rate)
                })
        });
    let Some(initial_config) = initial_config else {
        let message = format!(
            "No supported output configuration found for {} device '{}' on the audio worker thread",
            backend.label(),
            initial_device_name
        );
        error!("{}", message);
        let _ = ready_tx.send(Err(message));
        return;
    };

    let initial_output = match build_device_output(
        backend,
        &initial_device,
        initial_config,
        &initial_device_name,
        &output_settings_by_device,
        &output_peak_meter,
        performance,
    ) {
        Ok(output) => output,
        Err(error) => {
            let message = format!(
                "Could not open initial {} audio output stream for '{}' [{}]: {}",
                backend.label(),
                initial_device_name,
                device_id,
                error
            );
            error!("{}", message);
            let _ = ready_tx.send(Err(message));
            return;
        }
    };

    let _ = ready_tx.send(Ok((initial_output.channels, initial_output.sample_rate)));
    audio_worker_loop(
        backend,
        AudioOutputContext::Cpal(host),
        initial_output,
        initial_device_name,
        cmd_rx,
        pos_tx,
        output_settings_by_device,
        output_peak_meter,
        performance,
    );
}

#[cfg(target_os = "windows")]
fn audio_worker_directsound(
    selected_device: Option<String>,
    selected_endpoint: Option<AudioEndpoint>,
    include_full_device_metadata: bool,
    cmd_rx: Receiver<AudioCommand>,
    pos_tx: Sender<PositionUpdate>,
    output_settings_by_device: OutputSettingsByDevice,
    output_peak_meter: Arc<AtomicU32>,
    performance: AudioPerformanceSettings,
    ready_tx: Sender<Result<DirectSoundInitInfo, String>>,
) {
    let initialization = (|| -> anyhow::Result<_> {
        // SDL chooses its audio driver during SDL_Init. Set the driver before
        // creating the SDL context so this path is genuinely DirectSound.
        let hint_set = sdl2::hint::set_with_priority(
            "SDL_AUDIODRIVER",
            "directsound",
            &sdl2::hint::Hint::Override,
        );
        if !hint_set {
            anyhow::bail!("Could not force SDL2 to use the DirectSound audio driver");
        }
        let sdl = sdl2::init().map_err(|error| anyhow::anyhow!("SDL2 init failed: {error}"))?;
        let audio = sdl
            .audio()
            .map_err(|error| anyhow::anyhow!("SDL2 audio init failed: {error}"))?;
        let driver = audio.current_audio_driver();
        if !driver.eq_ignore_ascii_case("directsound") {
            anyhow::bail!(
                "SDL2 selected audio driver '{}', expected DirectSound. Check that the SDL2 build includes the DirectSound driver.",
                driver
            );
        }

        let scanned_devices =
            include_full_device_metadata.then(|| list_directsound_devices(&audio));
        let current_device = match selected_device {
            Some(selected) if include_full_device_metadata => scanned_devices
                .as_ref()
                .and_then(|devices| {
                    devices
                        .iter()
                        .find(|device| {
                            device.as_str() == selected
                                || AudioEngine::normalize_device_name(device.as_str())
                                    == AudioEngine::normalize_device_name(&selected)
                        })
                        .cloned()
                })
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Selected DirectSound output device '{}' is no longer available",
                        selected
                    )
                })?,
            Some(selected) => {
                let endpoint_matches = selected_endpoint.as_ref().is_some_and(|endpoint| {
                    endpoint.backend == AudioBackend::DirectSound
                        && endpoint.is_available()
                        && endpoint.device_name == selected
                });
                if selected != DIRECTSOUND_DEFAULT_DEVICE && !endpoint_matches {
                    anyhow::bail!(
                        "Selected DirectSound endpoint '{}' is missing from the cached device registry; refusing to enumerate devices during GO",
                        selected
                    );
                }
                selected
            }
            None => DIRECTSOUND_DEFAULT_DEVICE.to_string(),
        };
        let output_devices = scanned_devices.unwrap_or_else(|| vec![current_device.clone()]);
        let current_device_spec = if include_full_device_metadata
            || current_device == DIRECTSOUND_DEFAULT_DEVICE
        {
            directsound_device_spec(&audio, &current_device)
        } else {
            let endpoint = selected_endpoint.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "DirectSound endpoint '{}' has no cached channel metadata; refusing to scan devices during GO",
                    current_device
                )
            })?;
            (
                endpoint.channels.max(1),
                performance.sample_rate.unwrap_or(48_000).max(8_000),
            )
        };
        let initial_output = build_directsound_device_output_with_spec(
            &current_device,
            &output_settings_by_device,
            &output_peak_meter,
            performance,
            current_device_spec,
        )
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Could not open DirectSound output '{}' using sample rate {:?} and buffer {:?}",
                current_device,
                performance.sample_rate,
                performance.buffer_size_frames
            )
        })?;

        let mut output_device_channels = HashMap::new();
        let mut output_device_channel_layouts = HashMap::new();
        if include_full_device_metadata {
            for name in &output_devices {
                let (channels, _) = directsound_device_spec(&audio, name);
                output_device_channels.insert(name.clone(), channels.max(1));
                output_device_channel_layouts.insert(name.clone(), vec![channels.max(1)]);
            }
        } else if let Some(endpoint) = selected_endpoint.as_ref() {
            output_device_channels.insert(current_device.clone(), endpoint.channels.max(1));
            output_device_channel_layouts.insert(
                current_device.clone(),
                if endpoint.channel_layouts.is_empty() {
                    vec![endpoint.channels.max(1)]
                } else {
                    endpoint.channel_layouts.clone()
                },
            );
        } else {
            output_device_channels.insert(current_device.clone(), initial_output.channels.max(1));
            output_device_channel_layouts
                .insert(current_device.clone(), vec![initial_output.channels.max(1)]);
        }

        let ready = DirectSoundInitInfo {
            output_devices,
            current_device: current_device.clone(),
            output_channels: initial_output.channels,
            sample_rate: initial_output.sample_rate,
            output_device_channels,
            output_device_channel_layouts,
        };
        let context = AudioOutputContext::DirectSound { _sdl: sdl, audio };
        Ok((context, initial_output, ready, current_device))
    })();

    match initialization {
        Ok((context, initial_output, ready, current_device)) => {
            let _ = ready_tx.send(Ok(ready));
            info!("DirectSound audio worker initialized successfully");
            audio_worker_loop(
                AudioBackend::DirectSound,
                context,
                initial_output,
                current_device,
                cmd_rx,
                pos_tx,
                output_settings_by_device,
                output_peak_meter,
                performance,
            );
        }
        Err(error) => {
            error!("Could not initialize DirectSound output: {error:#}");
            let _ = ready_tx.send(Err(format!("{error:#}")));
        }
    }
}

fn detect_output_stream_failure(
    backend: AudioBackend,
    streams: &mut HashMap<String, DeviceOutput>,
    now: Instant,
) -> Option<(String, String)> {
    for (device_name, output) in streams.iter_mut() {
        let Some(health) = output.stream_health.as_ref() else {
            continue;
        };

        if health.stream_error.load(Ordering::Acquire) {
            return Some((
                device_name.clone(),
                "native audio backend reported an output-stream error".to_string(),
            ));
        }

        let callback_count = health.callback_count.load(Ordering::Relaxed);
        if callback_count != output.last_callback_count {
            output.last_callback_count = callback_count;
            output.last_callback_progress_at = now;
        } else if backend == AudioBackend::Asio {
            let timeout = if output.last_callback_count == 0 {
                ASIO_STREAM_INITIAL_CALLBACK_TIMEOUT
            } else {
                ASIO_STREAM_CALLBACK_STALL_TIMEOUT
            };
            let stalled_for = now.saturating_duration_since(output.last_callback_progress_at);
            if stalled_for >= timeout {
                return Some((
                    device_name.clone(),
                    format!(
                        "ASIO output callback made no progress for {:.0} ms",
                        stalled_for.as_secs_f64() * 1_000.0
                    ),
                ));
            }
        }
    }

    None
}

fn register_stop_callback_ack(
    streams: &mut HashMap<String, DeviceOutput>,
    device_names: impl IntoIterator<Item = String>,
    request_id: Uuid,
    cue_id: Option<Uuid>,
    requested_at: Instant,
) {
    let mut seen = std::collections::HashSet::new();
    for device_name in device_names {
        if !seen.insert(device_name.clone()) {
            continue;
        }
        let Some(output) = streams.get_mut(&device_name) else {
            continue;
        };
        let Some(health) = output.stream_health.as_ref() else {
            #[cfg(target_os = "windows")]
            if let OutputStreamHandle::DirectSound(queue) = &output._stream {
                let queued_bytes = queue.queued_bytes() as u64;
                let bytes_per_frame = (queue.channels.max(1) * std::mem::size_of::<f32>()) as u64;
                let queued_frames = queued_bytes / bytes_per_frame.max(1);
                let queued_ms = queued_frames as f64 * 1_000.0 / queue.sample_rate.max(1) as f64;
                info!(
                    "STOP_LATENCY: request_id={}, cue_id={:?}, endpoint='{}', stage=directsound_queue_observed, queued_bytes={}, queued_frames={}, queued_ms={:.2}",
                    request_id,
                    cue_id,
                    device_name,
                    queued_bytes,
                    queued_frames,
                    queued_ms
                );
            }
            info!(
                "STOP_LATENCY: request_id={}, cue_id={:?}, endpoint='{}', stage=callback_ack_unavailable, backend=DirectSound",
                request_id, cue_id, device_name
            );
            continue;
        };
        if output
            .pending_stop_callback_acks
            .iter()
            .any(|pending| pending.request_id == request_id)
        {
            continue;
        }
        let generation = health
            .stop_request_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        output
            .pending_stop_callback_acks
            .push_back(PendingStopCallbackAck {
                generation,
                request_id,
                cue_id,
                requested_at,
            });
    }
}

fn poll_output_stream_diagnostics(streams: &mut HashMap<String, DeviceOutput>, now: Instant) {
    for (device_name, output) in streams.iter_mut() {
        let Some(health) = output.stream_health.as_ref() else {
            continue;
        };

        let acknowledged_generation = health.stop_callback_ack_generation.load(Ordering::Acquire);
        if acknowledged_generation > output.last_stop_callback_ack_generation {
            while output
                .pending_stop_callback_acks
                .front()
                .is_some_and(|pending| pending.generation <= acknowledged_generation)
            {
                if let Some(pending) = output.pending_stop_callback_acks.pop_front() {
                    info!(
                        "STOP_LATENCY: request_id={}, cue_id={:?}, endpoint='{}', stage=audio_callback_buffer_rendered, stop_to_callback_observed_us={}",
                        pending.request_id,
                        pending.cue_id,
                        device_name,
                        pending.requested_at.elapsed().as_micros()
                    );
                    output.last_stop_callback_ack_generation = pending.generation;
                }
            }
            output.last_stop_callback_ack_generation = output
                .last_stop_callback_ack_generation
                .max(acknowledged_generation);
        }

        while output
            .pending_stop_callback_acks
            .front()
            .is_some_and(|pending| pending.requested_at.elapsed() >= Duration::from_secs(2))
        {
            if let Some(pending) = output.pending_stop_callback_acks.pop_front() {
                warn!(
                    "STOP_LATENCY: request_id={}, cue_id={:?}, endpoint='{}', stage=audio_callback_ack_timeout, elapsed_us={}",
                    pending.request_id,
                    pending.cue_id,
                    device_name,
                    pending.requested_at.elapsed().as_micros()
                );
            }
        }

        if now.saturating_duration_since(output.last_callback_metrics_at) < Duration::from_secs(5) {
            continue;
        }

        let callback_count = health.callback_count.load(Ordering::Relaxed);
        let total_ns = health.callback_total_ns.load(Ordering::Relaxed);
        let max_ns = health.callback_max_ns.load(Ordering::Relaxed);
        let over_budget = health.callback_over_budget_count.load(Ordering::Relaxed);
        let buffer_frames = health.callback_last_frames.load(Ordering::Relaxed);
        let callback_delta = callback_count.saturating_sub(output.last_callback_metrics_count);
        let total_delta_ns = total_ns.saturating_sub(output.last_callback_total_ns);
        let over_budget_delta = over_budget.saturating_sub(output.last_callback_over_budget_count);
        if callback_delta > 0 {
            info!(
                "AUDIO_CALLBACK_METRICS: endpoint='{}', callbacks={}, avg_processing_us={:.2}, max_processing_us_since_stream_open={}, over_budget_callbacks={}, buffer_frames={}, buffer_period_ms={:.2}, sample_rate={} Hz, channels={}",
                device_name,
                callback_delta,
                total_delta_ns as f64 / callback_delta as f64 / 1_000.0,
                max_ns / 1_000,
                over_budget_delta,
                buffer_frames,
                buffer_frames as f64 * 1_000.0 / output.sample_rate.max(1) as f64,
                output.sample_rate,
                output.channels
            );
        }
        let router_samples = health
            .channel_router_profile_samples
            .swap(0, Ordering::Relaxed);
        let router_profiled_frames = health
            .channel_router_profiled_frames
            .swap(0, Ordering::Relaxed);
        let router_source_total_ns = health
            .channel_router_source_total_ns
            .swap(0, Ordering::Relaxed);
        let router_source_max_ns = health
            .channel_router_source_max_ns
            .swap(0, Ordering::Relaxed);
        let router_route_total_ns = health
            .channel_router_route_total_ns
            .swap(0, Ordering::Relaxed);
        let router_route_max_ns = health
            .channel_router_route_max_ns
            .swap(0, Ordering::Relaxed);
        let router_active_instances = health
            .channel_router_active_instances
            .load(Ordering::Relaxed);

        if router_samples > 0 && router_profiled_frames > 0 {
            let router_estimated_ns = router_source_total_ns.saturating_add(router_route_total_ns);
            let unattributed_ns = total_delta_ns.saturating_sub(router_estimated_ns);
            info!(
                "CHANNEL_ROUTER_PROFILE: endpoint='{}', active_instances={}, sampled_blocks={}, estimated_cue_frames={}, source_estimated_ms={:.2}, source_avg_us_per_cue_frame={:.3}, source_max_sample_us={:.2}, route_estimated_ms={:.2}, route_avg_us_per_cue_frame={:.3}, route_max_sample_us={:.2}, callback_total_ms={:.2}, callback_unattributed_ms={:.2}",
                device_name,
                router_active_instances,
                router_samples,
                router_profiled_frames,
                router_source_total_ns as f64 / 1_000_000.0,
                router_source_total_ns as f64 / router_profiled_frames as f64 / 1_000.0,
                router_source_max_ns as f64 / 1_000.0,
                router_route_total_ns as f64 / 1_000_000.0,
                router_route_total_ns as f64 / router_profiled_frames as f64 / 1_000.0,
                router_route_max_ns as f64 / 1_000.0,
                total_delta_ns as f64 / 1_000_000.0,
                unattributed_ns as f64 / 1_000_000.0,
            );
        }

        let mixer_profile_samples = health
            .mixer_output_profile_samples
            .swap(0, Ordering::Relaxed);
        let mixer_profiled_frames = health
            .mixer_output_profiled_frames
            .swap(0, Ordering::Relaxed);
        let mixer_output_total_ns = health.mixer_output_total_ns.swap(0, Ordering::Relaxed);
        let mixer_output_max_ns = health.mixer_output_max_ns.swap(0, Ordering::Relaxed);

        if mixer_profile_samples > 0 && mixer_profiled_frames > 0 {
            let router_estimated_ns = router_source_total_ns.saturating_add(router_route_total_ns);
            let mixer_unattributed_ns = mixer_output_total_ns.saturating_sub(router_estimated_ns);
            let post_mixer_ns = total_delta_ns.saturating_sub(mixer_output_total_ns);
            info!(
                "MIXER_OUTPUT_PROFILE: endpoint='{}', active_instances={}, sampled_frames={}, estimated_endpoint_frames={}, mixer_estimated_ms={:.2}, mixer_avg_us_per_endpoint_frame={:.3}, mixer_max_sample_us={:.2}, router_source_estimated_ms={:.2}, router_route_estimated_ms={:.2}, mixer_unattributed_ms={:.2}, post_mixer_estimated_ms={:.2}, callback_total_ms={:.2}",
                device_name,
                router_active_instances,
                mixer_profile_samples,
                mixer_profiled_frames,
                mixer_output_total_ns as f64 / 1_000_000.0,
                mixer_output_total_ns as f64 / mixer_profiled_frames as f64 / 1_000.0,
                mixer_output_max_ns as f64 / 1_000.0,
                router_source_total_ns as f64 / 1_000_000.0,
                router_route_total_ns as f64 / 1_000_000.0,
                mixer_unattributed_ns as f64 / 1_000_000.0,
                post_mixer_ns as f64 / 1_000_000.0,
                total_delta_ns as f64 / 1_000_000.0,
            );
        }

        output.last_callback_metrics_count = callback_count;
        output.last_callback_total_ns = total_ns;
        output.last_callback_over_budget_count = over_budget;
        output.last_callback_metrics_at = now;
    }
}

fn audio_worker_loop(
    backend: AudioBackend,
    context: AudioOutputContext,
    initial_output: DeviceOutput,
    initial_device_name: String,
    cmd_rx: Receiver<AudioCommand>,
    pos_tx: Sender<PositionUpdate>,
    output_settings_by_device: OutputSettingsByDevice,
    output_peak_meter: Arc<AtomicU32>,
    mut performance: AudioPerformanceSettings,
) {
    performance = AudioPerformanceSettings::for_backend(backend, performance);
    let mut streams = HashMap::new();
    streams.insert(initial_device_name.clone(), initial_output);
    let mut default_device_name = initial_device_name;
    let mut players: Vec<ActivePlayer> = Vec::new();
    let (retired_automation_points_tx, retired_automation_points_rx) = bounded(1_024);
    let test_generation = Arc::new(AtomicU64::new(0));
    let tick = Duration::from_millis(5);

    loop {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                AudioCommand::Shutdown => {
                    for player in &players {
                        let _ = pos_tx.try_send(PositionUpdate {
                            id: player.id,
                            generation: player.generation,
                            position_secs: player.current_pos(),
                            state: CueState::Stopped,
                        });
                        player.sink.stop();
                    }
                    players.clear();
                    test_generation.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                AudioCommand::ReconfigureOutput {
                    performance: requested_performance,
                    reply,
                } => {
                    let requested_performance =
                        AudioPerformanceSettings::for_backend(backend, requested_performance);
                    let previous_performance = performance;
                    test_generation.fetch_add(1, Ordering::Relaxed);
                    for player in &players {
                        let _ = pos_tx.try_send(PositionUpdate {
                            id: player.id,
                            generation: player.generation,
                            position_secs: player.current_pos(),
                            state: CueState::Stopped,
                        });
                        player.sink.stop();
                    }
                    players.clear();
                    // Drop the current native stream before reopening the same
                    // endpoint. ASIO drivers commonly allow only one stream owner.
                    streams.clear();

                    if let Some((_, channels, sample_rate, settings)) = ensure_device_output(
                        backend,
                        &mut streams,
                        &default_device_name,
                        &context,
                        &output_settings_by_device,
                        &output_peak_meter,
                        requested_performance,
                    ) {
                        let mut current = settings.write();
                        current.resize(channels, OutputChannelSetting::default());
                        current.truncate(channels);
                        performance = requested_performance;
                        info!(
                            "Reconfigured active output '{}': {} channels @ {} Hz, performance={:?}",
                            default_device_name,
                            channels,
                            sample_rate,
                            performance
                        );
                        let _ = reply.send(Ok((channels, sample_rate)));
                    } else {
                        streams.clear();
                        let restored = ensure_device_output(
                            backend,
                            &mut streams,
                            &default_device_name,
                            &context,
                            &output_settings_by_device,
                            &output_peak_meter,
                            previous_performance,
                        );
                        if restored.is_some() {
                            warn!(
                                "Could not apply performance settings {:?} to '{}'; previous settings {:?} were restored",
                                requested_performance,
                                default_device_name,
                                previous_performance
                            );
                        } else {
                            error!(
                                "Could not apply performance settings {:?} to '{}' and could not restore previous settings {:?}",
                                requested_performance,
                                default_device_name,
                                previous_performance
                            );
                        }
                        let _ = reply.send(Err(format!(
                            "Could not apply audio performance settings {:?} to '{}'. Previous settings {}.",
                            requested_performance,
                            default_device_name,
                            if restored.is_some() { "were restored" } else { "could not be restored" }
                        )));
                    }
                }
                AudioCommand::SetOutputDevice {
                    device_name: dev_name,
                    endpoint_id: _,
                    performance: requested_performance,
                    reply,
                } => {
                    let requested_performance =
                        AudioPerformanceSettings::for_backend(backend, requested_performance);
                    let previous_default_device = default_device_name.clone();
                    test_generation.fetch_add(1, Ordering::Relaxed);

                    if backend == AudioBackend::Asio && dev_name != previous_default_device {
                        // Legacy in-process path: CPAL's ASIO host owns one driver
                        // at a time per process. The GUI path switches endpoint
                        // workers in the parent manager instead of using this branch.
                        for player in &players {
                            let _ = pos_tx.try_send(PositionUpdate {
                                id: player.id,
                                generation: player.generation,
                                position_secs: player.current_pos(),
                                state: CueState::Stopped,
                            });
                            player.sink.stop();
                        }
                        players.clear();
                        streams.clear();
                        info!(
                            "ASIO endpoint switch: released previous driver stream(s) before opening '{}'",
                            dev_name
                        );
                    } else {
                        // WASAPI/DirectSound can keep other per-Cue streams alive.
                        players.retain(|player| {
                            if player.uses_global_default {
                                info!(
                                    "Stopping cue {} on previous global output '{}'",
                                    player.id, player.output_device
                                );
                                let _ = pos_tx.try_send(PositionUpdate {
                                    id: player.id,
                                    generation: player.generation,
                                    position_secs: player.current_pos(),
                                    state: CueState::Stopped,
                                });
                                player.sink.stop();
                                false
                            } else {
                                true
                            }
                        });
                    }

                    default_device_name = dev_name.clone();
                    if let Some((_, channels, sample_rate, settings)) = ensure_device_output(
                        backend,
                        &mut streams,
                        &dev_name,
                        &context,
                        &output_settings_by_device,
                        &output_peak_meter,
                        requested_performance,
                    ) {
                        let mut current = settings.write();
                        current.resize(channels, OutputChannelSetting::default());
                        current.truncate(channels);
                        performance = requested_performance;
                        info!(
                            "Switched global output device to: {} — {} ({} channels @ {} Hz; performance={:?})",
                            backend.short_label(),
                            dev_name,
                            channels,
                            sample_rate,
                            performance
                        );
                        let _ = reply.send(Ok((channels, sample_rate)));
                    } else {
                        warn!(
                            "Could not open selected {} output configuration for device: {}",
                            backend.short_label(),
                            dev_name
                        );

                        default_device_name = previous_default_device.clone();
                        let restored = if backend == AudioBackend::Asio
                            && previous_default_device != dev_name
                        {
                            ensure_device_output(
                                backend,
                                &mut streams,
                                &previous_default_device,
                                &context,
                                &output_settings_by_device,
                                &output_peak_meter,
                                performance,
                            )
                        } else {
                            streams.get_mut(&previous_default_device).map(|output| {
                                output.settings = shared_output_settings(
                                    &output_settings_by_device,
                                    &previous_default_device,
                                    output.channels,
                                );
                                (
                                    output.mixer.clone(),
                                    output.channels,
                                    output.sample_rate,
                                    output.settings.clone(),
                                )
                            })
                        };

                        if restored.is_some() {
                            warn!(
                                "Restored previous {} output '{}' after the requested device failed to open",
                                backend.short_label(),
                                previous_default_device
                            );
                        } else {
                            error!(
                                "Could not restore previous {} output '{}' after a failed device switch",
                                backend.short_label(),
                                previous_default_device
                            );
                        }

                        let _ = reply.send(Err(format!(
                            "Could not open {} output device '{}'. The previous endpoint '{}' {}.",
                            backend.short_label(),
                            dev_name,
                            previous_default_device,
                            if restored.is_some() {
                                "was restored"
                            } else {
                                "could not be restored"
                            }
                        )));
                    }
                }
                AudioCommand::SetInputDevice(dev_name) => {
                    info!("Selected input audio device: {}", dev_name);
                }
                AudioCommand::PlayBatch {
                    transaction_id,
                    cues,
                } => handle_play_batch(
                    transaction_id,
                    cues,
                    backend,
                    &mut players,
                    &mut streams,
                    &context,
                    &default_device_name,
                    &output_settings_by_device,
                    &output_peak_meter,
                    performance,
                    &test_generation,
                    &pos_tx,
                    &retired_automation_points_tx,
                ),
                other => handle_command(
                    other,
                    backend,
                    &mut players,
                    &mut streams,
                    &context,
                    &default_device_name,
                    &output_settings_by_device,
                    &output_peak_meter,
                    performance,
                    &test_generation,
                    &pos_tx,
                    None,
                    None,
                    &retired_automation_points_tx,
                ),
            }
        }

        while let Ok(retired_points) = retired_automation_points_rx.try_recv() {
            drop(retired_points);
        }

        let diagnostics_now = Instant::now();
        poll_output_stream_diagnostics(&mut streams, diagnostics_now);

        if let Some((device_name, reason)) =
            detect_output_stream_failure(backend, &mut streams, diagnostics_now)
        {
            let message = format!(
                "{} output stream '{}' failed: {}",
                backend.short_label(),
                device_name,
                reason
            );
            error!(
                "Audio Worker render loop is stopping because its native output stream is unhealthy: {}",
                message
            );
            for player in &players {
                let _ = pos_tx.try_send(PositionUpdate {
                    id: player.id,
                    generation: player.generation,
                    position_secs: player.current_pos(),
                    state: CueState::Error(message.clone()),
                });
                player.sink.stop();
            }
            return;
        }

        let mut finished_ids: Vec<(Uuid, u64)> = Vec::new();
        let mut stopped_ids: Vec<(Uuid, u64)> = Vec::new();
        let mut completed_stop_acks: Vec<(PendingStopCompletion, String)> = Vec::new();
        for player in &mut players {
            if player.stopping {
                if player.stop_fade.is_completed() || player.sink.empty() {
                    player.sink.stop();
                    let device_name = player.output_device.clone();
                    for pending in player.pending_stop_completions.drain(..) {
                        completed_stop_acks.push((pending, device_name.clone()));
                    }
                    stopped_ids.push((player.id, player.generation));
                }
                continue;
            }

            let prefetch_underruns = player.prefetch_underrun_count.load(Ordering::Relaxed);
            let prefetch_underrun_frames =
                player.prefetch_underrun_frame_count.load(Ordering::Relaxed);
            if prefetch_underruns > player.last_prefetch_underrun_count
                || prefetch_underrun_frames > player.last_prefetch_underrun_frame_count
            {
                let new_episodes =
                    prefetch_underruns.saturating_sub(player.last_prefetch_underrun_count);
                let new_silent_frames = prefetch_underrun_frames
                    .saturating_sub(player.last_prefetch_underrun_frame_count);
                let sample_rate = player.source_sample_rate.max(1) as f64;
                warn!(
                    "AUDIO_PREFETCH_UNDERRUN: cue_id={}, generation={}, endpoint='{}', new_episodes={}, total_episodes={}, new_silent_frames={}, total_silent_frames={}, new_silence_ms={:.3}, total_silence_ms={:.3}, source_rate={} Hz",
                    player.id,
                    player.generation,
                    player.output_device,
                    new_episodes,
                    prefetch_underruns,
                    new_silent_frames,
                    prefetch_underrun_frames,
                    new_silent_frames as f64 * 1000.0 / sample_rate,
                    prefetch_underrun_frames as f64 * 1000.0 / sample_rate,
                    player.source_sample_rate
                );
                player.last_prefetch_underrun_count = prefetch_underruns;
                player.last_prefetch_underrun_frame_count = prefetch_underrun_frames;
            }

            if player_is_paused(&streams, player) {
                let _ = pos_tx.try_send(PositionUpdate {
                    id: player.id,
                    generation: player.generation,
                    position_secs: player.current_pos(),
                    state: CueState::Paused,
                });
                continue;
            }

            let pos = player.current_pos();

            if !player.start_frame_reported {
                if let Some(observed) = &player.start_frame_observed {
                    let actual_start_frame = observed.load(Ordering::Acquire);
                    if actual_start_frame != u64::MAX {
                        info!(
                            "Cue {} actual output start frame: {} (generation={}, transaction_id={:?})",
                            player.id,
                            actual_start_frame,
                            player.generation,
                            player.launch_transaction_id
                        );
                        player.start_frame_reported = true;
                    }
                }
            }

            // Loop boundaries are handled inside the continuous PCM source.
            // The Worker never stops or recreates the Player/Sink for a repeat.
            let completed_loops = player.loop_iterations.load(Ordering::Relaxed);
            if completed_loops > player.last_reported_loop_iterations
                && (completed_loops == 1
                    || completed_loops.saturating_sub(player.last_reported_loop_iterations) >= 10)
            {
                info!(
                    "AUDIO_LOOP_PRODUCER_PROGRESS: cue_id={}, generation={}, prepared_repeats={}, producer_loops_remaining={}",
                    player.id,
                    player.generation,
                    completed_loops,
                    player.loops_remaining.load(Ordering::Acquire)
                );
                player.last_reported_loop_iterations = completed_loops;
            }

            // This counter changes only when the render-side PCM consumer reaches
            // a Loop wrap. It is deliberately separate from the producer counter
            // above, which can run ahead by the entire prefetch queue.
            let rendered_loops = player.render_loop_iterations.load(Ordering::Acquire);
            if rendered_loops > player.last_reported_render_loop_iterations {
                let new_repeats =
                    rendered_loops.saturating_sub(player.last_reported_render_loop_iterations);
                let boundary_frame = player.render_loop_boundary_frame.load(Ordering::Acquire);
                let boundary_position = f64::from_bits(
                    player
                        .render_loop_boundary_position_bits
                        .load(Ordering::Acquire),
                );
                let endpoint_render_frame = player
                    .render_loop_boundary_endpoint_frame
                    .load(Ordering::Acquire);
                let endpoint_render_frame =
                    (endpoint_render_frame != u64::MAX).then_some(endpoint_render_frame);
                let source_media_frame = if boundary_position.is_finite() && boundary_position > 0.0
                {
                    (boundary_position * f64::from(player.source_sample_rate)).round() as u64
                } else {
                    0
                };
                info!(
                    "AUDIO_LOOP_RENDERED: cue_id={}, generation={}, rendered_repeats={}, new_repeats={}, source_frame={}, source_media_frame={}, endpoint_render_frame={:?}, media_position_secs={:.6}, source_rate={} Hz, endpoint_rate={} Hz",
                    player.id,
                    player.generation,
                    rendered_loops,
                    new_repeats,
                    boundary_frame,
                    source_media_frame,
                    endpoint_render_frame,
                    boundary_position,
                    player.source_sample_rate,
                    player.output_sample_rate
                );
                player.last_reported_render_loop_iterations = rendered_loops;
            }

            let fingerprint_iteration = player
                .render_loop_fingerprint_iteration
                .load(Ordering::Acquire);
            if fingerprint_iteration > player.last_reported_render_loop_fingerprint_iteration {
                let fingerprint = player.render_loop_fingerprint.load(Ordering::Acquire);
                let boundary_position = f64::from_bits(
                    player
                        .render_loop_boundary_position_bits
                        .load(Ordering::Acquire),
                );
                let source_media_frame = if boundary_position.is_finite() && boundary_position > 0.0
                {
                    (boundary_position * f64::from(player.source_sample_rate)).round() as u64
                } else {
                    0
                };
                info!(
                    "AUDIO_LOOP_PCM_FINGERPRINT: cue_id={}, generation={}, loop_iteration={}, source_media_frame={}, frames={}, fnv1a64=0x{:016x}",
                    player.id,
                    player.generation,
                    fingerprint_iteration,
                    source_media_frame,
                    PCM_LOOP_FINGERPRINT_FRAMES,
                    fingerprint
                );
                player.last_reported_render_loop_fingerprint_iteration = fingerprint_iteration;
            }

            let capture_iteration = player
                .loop_pcm_capture
                .completed_iteration
                .load(Ordering::Acquire);
            if capture_iteration > player.last_reported_loop_pcm_capture_iteration {
                let capture = &player.loop_pcm_capture;
                let channels = capture.channels.load(Ordering::Relaxed) as usize;
                let pre_frames_valid = capture.pre_frames_valid.load(Ordering::Relaxed);
                let post_samples_captured = capture.post_samples_captured.load(Ordering::Relaxed);
                let capture_status = match capture.status.load(Ordering::Relaxed) {
                    PCM_LOOP_CAPTURE_STATUS_VALID => "valid",
                    PCM_LOOP_CAPTURE_STATUS_UNDERRUN => "underrun",
                    PCM_LOOP_CAPTURE_STATUS_UNSUPPORTED_CHANNELS => "unsupported_channels",
                    PCM_LOOP_CAPTURE_STATUS_INCOMPLETE => "incomplete",
                    _ => "pending_or_unknown",
                };
                let endpoint_frame = capture.endpoint_render_frame.load(Ordering::Relaxed);
                let endpoint_frame = (endpoint_frame != u64::MAX).then_some(endpoint_frame);
                let (pre_samples_bits, post_samples_bits) =
                    if channels > 0 && channels <= PCM_LOOP_CAPTURE_MAX_CHANNELS {
                        let pre = (0..PCM_LOOP_CAPTURE_PRE_FRAMES * channels)
                            .map(|index| {
                                format!("{:08x}", capture.samples[index].load(Ordering::Relaxed))
                            })
                            .collect::<Vec<_>>()
                            .join(",");
                        let post_start = PCM_LOOP_CAPTURE_PRE_FRAMES * channels;
                        let captured_post_samples = (post_samples_captured as usize)
                            .min(PCM_LOOP_CAPTURE_POST_FRAMES * channels);
                        let post = (0..captured_post_samples)
                            .map(|index| {
                                format!(
                                    "{:08x}",
                                    capture.samples[post_start + index].load(Ordering::Relaxed)
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(",");
                        (pre, post)
                    } else {
                        ("unavailable".to_string(), "unavailable".to_string())
                    };

                info!(
                    "AUDIO_LOOP_PCM_WINDOW: cue_id={}, generation={}, loop_iteration={}, source_elapsed_frame={}, source_media_frame={}, endpoint_render_frame={:?}, source_rate={} Hz, endpoint_rate={} Hz, channels={}, pre_frames_valid={}, post_samples_captured={}, status={}, dropped_captures={}, pre_f32_bits=[{}], post_f32_bits=[{}]",
                    player.id,
                    player.generation,
                    capture.iteration.load(Ordering::Relaxed),
                    capture.source_elapsed_frame.load(Ordering::Relaxed),
                    capture.source_media_frame.load(Ordering::Relaxed),
                    endpoint_frame,
                    player.source_sample_rate,
                    player.output_sample_rate,
                    channels,
                    pre_frames_valid,
                    post_samples_captured,
                    capture_status,
                    capture.dropped_captures.load(Ordering::Relaxed),
                    pre_samples_bits,
                    post_samples_bits
                );
                player.last_reported_loop_pcm_capture_iteration = capture_iteration;
                capture
                    .acknowledged_iteration
                    .store(capture_iteration, Ordering::Release);
            }

            if (player.end_secs > 0.0 && pos >= player.end_secs) || player.sink.empty() {
                player.sink.stop();
                finished_ids.push((player.id, player.generation));
                continue;
            }

            let mut state = CueState::Playing;
            let mut env = 1.0_f32;
            let elapsed_from_start = if player.loop_runtime_active {
                player.playback_elapsed_frames.load(Ordering::Relaxed) as f64
                    / player.source_sample_rate.max(1) as f64
            } else {
                (pos - player.start_secs).max(0.0)
            };
            if player.fade_in_secs > 0.0 && elapsed_from_start < player.fade_in_secs {
                let t = (elapsed_from_start / player.fade_in_secs) as f32;
                env *= player
                    .fade_curve
                    .apply_with_points(t, &player.custom_fade_points);
                state = CueState::FadingIn;
            }
            let is_final_loop_pass = should_apply_loop_fade_out(
                player.loop_runtime_active,
                player.render_loop_remaining_state.load(Ordering::Acquire),
                player.end_secs,
                player.loop_end,
            );
            if is_final_loop_pass && player.end_secs > 0.0 && player.fade_out_secs > 0.0 {
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
                        // The control-thread fade may finish between worker ticks.
                        // Finish with the same sample-level de-click ramp used by Stop.
                        player.stopping = true;
                        player.stop_fade.request();
                        let _ = pos_tx.try_send(PositionUpdate {
                            id: player.id,
                            generation: player.generation,
                            position_secs: pos,
                            state: CueState::Stopped,
                        });
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
                        generation: player.generation,
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
                generation: player.generation,
                position_secs: pos,
                state,
            });
        }

        for (stopped_id, generation) in stopped_ids {
            players.retain(|player| player.id != stopped_id || player.generation != generation);
        }

        for (finished_id, generation) in finished_ids {
            players.retain(|player| player.id != finished_id || player.generation != generation);
            let _ = pos_tx.try_send(PositionUpdate {
                id: finished_id,
                generation,
                position_secs: 0.0,
                state: CueState::Stopped,
            });
        }

        let mut registered_stop_ack_keys = std::collections::HashSet::new();
        for (pending, device_name) in completed_stop_acks {
            let still_waiting_on_device = players.iter().any(|player| {
                player.output_device == device_name
                    && player
                        .pending_stop_completions
                        .iter()
                        .any(|other| other.request_id == pending.request_id)
            });
            if still_waiting_on_device
                || !registered_stop_ack_keys
                    .insert((pending.request_id.clone(), device_name.clone()))
            {
                continue;
            }

            info!(
                "STOP_LATENCY: request_id={}, cue_id={:?}, endpoint='{}', stage=stop_fade_rendered, stop_to_fade_complete_observed_us={}, fade_ms={:.2}",
                pending.request_id,
                pending.cue_id,
                device_name,
                pending.requested_at.elapsed().as_micros(),
                STOP_DECLICK_FADE_MS
            );
            register_stop_callback_ack(
                &mut streams,
                std::iter::once(device_name),
                pending.request_id.clone(),
                pending.cue_id.clone(),
                pending.requested_at.clone(),
            );
        }

        #[cfg(target_os = "windows")]
        pump_directsound_queues(&mut streams);

        std::thread::sleep(tick);
    }
}

fn handle_play_batch(
    transaction_id: Uuid,
    cues: Vec<AudioPlayRequest>,
    backend: AudioBackend,
    players: &mut Vec<ActivePlayer>,
    streams: &mut HashMap<String, DeviceOutput>,
    context: &AudioOutputContext,
    default_device_name: &str,
    output_settings_by_device: &OutputSettingsByDevice,
    output_peak_meter: &Arc<AtomicU32>,
    performance: AudioPerformanceSettings,
    test_generation: &Arc<AtomicU64>,
    pos_tx: &Sender<PositionUpdate>,
    retired_automation_points_tx: &Sender<Vec<[f64; 2]>>,
) {
    if cues.is_empty() {
        return;
    }

    let batch_size = cues.len();
    let cue_ids: Vec<Uuid> = cues.iter().map(|cue| cue.id).collect();

    // Make sure the endpoint clock exists before preparing the batch. In the
    // normal case the Worker already owns an open stream, so this is a lookup.
    let output_ready = ensure_device_output(
        backend,
        streams,
        default_device_name,
        context,
        output_settings_by_device,
        output_peak_meter,
        performance,
    )
    .is_some();

    let sample_frame_clock = output_ready
        .then(|| {
            streams
                .get(default_device_name)
                .map(|output| output.sample_frame_clock.clone())
        })
        .flatten();

    // MAX means "not armed". Every scheduled source emits silence until this
    // one shared gate is assigned after all decoders and queues are prepared.
    let start_frame_gate = Arc::new(AtomicU64::new(u64::MAX));

    for cue in cues {
        handle_command(
            AudioCommand::Play {
                id: cue.id,
                generation: cue.generation,
                file: cue.file,
                output_device: None,
                output_backend: None,
                output_device_id: None,
                start_secs: cue.start_secs,
                end_secs: cue.end_secs,
                duration_secs: cue.duration_secs,
                volume: cue.volume,
                volume_automation_enabled: cue.volume_automation_enabled,
                volume_automation: cue.volume_automation,
                time_pitch: cue.time_pitch,
                fade_in_secs: cue.fade_in_secs,
                fade_out_secs: cue.fade_out_secs,
                fade_curve: cue.fade_curve,
                custom_fade_points: cue.custom_fade_points,
                channel_routes: cue.channel_routes,
                output_gains_db: cue.output_gains_db,
                loop_enabled: cue.loop_enabled,
                loop_start: cue.loop_start,
                loop_end: cue.loop_end,
                loop_count: cue.loop_count,
            },
            backend,
            players,
            streams,
            context,
            default_device_name,
            output_settings_by_device,
            output_peak_meter,
            performance,
            test_generation,
            pos_tx,
            Some(start_frame_gate.clone()),
            Some(transaction_id),
            retired_automation_points_tx,
        );
    }

    if let Some(frame_clock) = sample_frame_clock {
        // Use at least one configured output buffer as a preparation margin.
        // The exact target is selected only after all sources have been attached.
        let configured_buffer_frames = streams
            .get(default_device_name)
            .and_then(|output| output.performance.buffer_size_frames)
            .unwrap_or(512) as u64;
        let preroll_frames = configured_buffer_frames.max(256);
        let current_frame = frame_clock.load(Ordering::Relaxed);
        let target_frame = current_frame.saturating_add(preroll_frames);
        start_frame_gate.store(target_frame, Ordering::Release);

        info!(
            "Sample-synchronized PlayBatch armed: transaction_id={}, cues={}, cue_ids={:?}, endpoint='{}', backend={}, sample_rate={} Hz, current_frame={}, target_frame={}, preroll={} frames ({:.2} ms)",
            transaction_id,
            batch_size,
            cue_ids,
            default_device_name,
            backend.short_label(),
            streams
                .get(default_device_name)
                .map(|output| output.sample_rate)
                .unwrap_or_default(),
            current_frame,
            target_frame,
            preroll_frames,
            preroll_frames as f64
                * 1_000.0
                / streams
                    .get(default_device_name)
                    .map(|output| output.sample_rate.max(1))
                    .unwrap_or(48_000) as f64
        );
    } else {
        // This fallback is only expected if the native endpoint cannot expose a
        // render-frame clock. The prepared sources are released immediately.
        warn!(
            "Sample-synchronized PlayBatch could not access an output frame clock for '{}'; cues will start immediately without sample alignment",
            default_device_name
        );
        start_frame_gate.store(0, Ordering::Release);
    }
}

fn handle_command(
    cmd: AudioCommand,
    backend: AudioBackend,
    players: &mut Vec<ActivePlayer>,
    streams: &mut HashMap<String, DeviceOutput>,
    context: &AudioOutputContext,
    default_device_name: &str,
    output_settings_by_device: &OutputSettingsByDevice,
    output_peak_meter: &Arc<AtomicU32>,
    performance: AudioPerformanceSettings,
    test_generation: &Arc<AtomicU64>,
    pos_tx: &Sender<PositionUpdate>,
    scheduled_start_gate: Option<Arc<AtomicU64>>,
    launch_transaction_id: Option<Uuid>,
    retired_automation_points_tx: &Sender<Vec<[f64; 2]>>,
) {
    match cmd {
        AudioCommand::Play {
            id,
            generation,
            file,
            output_device,
            output_backend: _,
            output_device_id: _,
            start_secs,
            end_secs,
            duration_secs,
            volume,
            volume_automation_enabled,
            mut volume_automation,
            mut time_pitch,
            fade_in_secs,
            fade_out_secs,
            fade_curve,
            custom_fade_points,
            channel_routes,
            output_gains_db,
            loop_enabled,
            loop_start,
            loop_end,
            loop_count,
        } => {
            let prepare_started = Instant::now();
            volume_automation.retain(|point| point[0].is_finite() && point[1].is_finite());
            volume_automation.sort_by(|a, b| a[0].total_cmp(&b[0]));
            for point in &mut volume_automation {
                point[0] = point[0].clamp(0.0, duration_secs.max(0.01));
                point[1] = point[1].clamp(0.0, 1.0);
            }
            sanitize_time_pitch_settings(&mut time_pitch);
            // A second GO on the same cue replaces its decoder from the start
            // marker. Do not hard-stop the previous sink at a non-zero sample:
            // give it a very short fade-out tail to avoid an audible click.
            let previous_generation = players
                .iter()
                .filter(|player| player.id == id)
                .map(|player| player.generation)
                .max();
            let mut replacing_existing = false;
            let restart_fade_at = Instant::now();
            players.retain_mut(|player| {
                if player.id != id {
                    return true;
                }
                replacing_existing = true;
                // An explicit Stop is already fading sample-by-sample. Keep that
                // short tail alive rather than cutting it off with a second GO.
                if player.stopping {
                    return true;
                }
                // Older restart tails are already near silence. Stop those and
                // retain only the newest instance for one clean crossfade tail.
                if Some(player.generation) != previous_generation
                    || player_is_paused(streams, player)
                    || player.current_volume <= 0.000_01
                {
                    player.sink.stop();
                    false
                } else {
                    player.dynamic_fade = Some(DynamicFade {
                        start_vol: player.current_volume,
                        target_vol: 0.0,
                        start_time: restart_fade_at,
                        duration_secs: 0.02,
                        stop_on_finish: true,
                        curve: FadeCurve::Linear,
                        custom_fade_points: vec![[0.0, 0.0], [1.0, 1.0]],
                    });
                    true
                }
            });

            let effective_fade_in_secs = if replacing_existing {
                fade_in_secs.max(0.02)
            } else {
                fade_in_secs
            };

            let uses_global_default = output_device.is_none();
            let device_name = output_device.unwrap_or_else(|| default_device_name.to_string());
            let Some((mixer, output_channels, sample_rate, output_settings)) = ensure_device_output(
                backend,
                streams,
                &device_name,
                context,
                output_settings_by_device,
                output_peak_meter,
                performance,
            ) else {
                warn!("Cannot open output device '{}' for cue {}", device_name, id);
                return;
            };
            let sample_frame_clock = streams
                .get(&device_name)
                .map(|output| output.sample_frame_clock.clone());
            let channel_router_profile = streams
                .get(&device_name)
                .and_then(|output| output.stream_health.clone());
            let start_frame_observed =
                if scheduled_start_gate.is_some() && sample_frame_clock.is_some() {
                    Some(Arc::new(AtomicU64::new(u64::MAX)))
                } else {
                    None
                };

            info!(
                "CUE OUTPUT ROUTING: cue={}, backend={}, device='{}', stream_channels={}, sample_rate={} Hz, routes={:?}",
                id, backend.short_label(), device_name, output_channels, sample_rate, channel_routes
            );
            let initial_volume = if effective_fade_in_secs > 0.0 {
                0.0
            } else {
                volume
            };
            let route_state = Arc::new(RwLock::new(channel_routes));
            let output_gains_state = Arc::new(RwLock::new(output_gains_db));
            {
                let mut gains = output_gains_state.write();
                gains.resize(output_channels, 0.0);
                gains.truncate(output_channels);
                for gain in gains.iter_mut() {
                    if !gain.is_finite() {
                        *gain = 0.0;
                    }
                    *gain = (*gain).clamp(-60.0, 18.0);
                }
            }
            let (automation_update_tx, automation_update_rx) = unbounded();
            let time_pitch_state = Arc::new(RwLock::new(time_pitch));
            let position_state = Arc::new(AtomicU64::new(start_secs.max(0.0).to_bits()));
            let prefetch_underrun_count = Arc::new(AtomicU64::new(0));
            let prefetch_underrun_frame_count = Arc::new(AtomicU64::new(0));
            let loops_remaining_state = Arc::new(AtomicI32::new(loop_count));
            let loop_iterations = Arc::new(AtomicU64::new(0));
            let render_loop_iterations = Arc::new(AtomicU64::new(0));
            let render_loop_boundary_frame = Arc::new(AtomicU64::new(0));
            let render_loop_boundary_position_bits =
                Arc::new(AtomicU64::new(start_secs.max(0.0).to_bits()));
            let render_loop_boundary_endpoint_frame = Arc::new(AtomicU64::new(u64::MAX));
            let loop_pcm_capture = Arc::new(LoopPcmCapture::new());
            let render_loop_fingerprint = Arc::new(AtomicU64::new(0));
            let render_loop_fingerprint_iteration = Arc::new(AtomicU64::new(0));
            let playback_elapsed_frames = Arc::new(AtomicU64::new(0));
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
            let loop_runtime_active = loop_enabled
                && effective_loop_end > loop_start
                && (loop_count == -1 || loop_count > 1);
            let render_loop_remaining_state = Arc::new(AtomicI32::new(if loop_runtime_active {
                loop_count
            } else {
                0
            }));
            let mut pump_during_prefetch = || {
                #[cfg(target_os = "windows")]
                pump_directsound_queues(streams);
            };
            let stop_fade = StopFadeControl::new();
            let sink_result = open_sink_at(
                &mixer,
                &file,
                start_secs,
                initial_volume,
                loop_enabled,
                loop_start,
                effective_loop_end,
                loops_remaining_state.clone(),
                loop_iterations.clone(),
                playback_elapsed_frames.clone(),
                render_loop_remaining_state.clone(),
                stop_fade.clone(),
                output_channels,
                sample_rate,
                route_state.clone(),
                output_settings.clone(),
                output_gains_state.clone(),
                channel_router_profile,
                volume_automation_enabled,
                volume_automation.clone(),
                automation_update_rx.clone(),
                retired_automation_points_tx.clone(),
                time_pitch_state.clone(),
                position_state.clone(),
                prefetch_underrun_count.clone(),
                prefetch_underrun_frame_count.clone(),
                render_loop_iterations.clone(),
                render_loop_boundary_frame.clone(),
                render_loop_boundary_position_bits.clone(),
                render_loop_boundary_endpoint_frame.clone(),
                loop_pcm_capture.clone(),
                render_loop_fingerprint.clone(),
                render_loop_fingerprint_iteration.clone(),
                &mut pump_during_prefetch,
                scheduled_start_gate.clone(),
                sample_frame_clock,
                start_frame_observed.clone(),
            );
            drop(pump_during_prefetch);
            let Some((sink, source_sample_rate)) = sink_result else {
                warn!("Cannot open audio file for playback: {:?}", file);
                return;
            };
            info!(
                "Cue {} output device + decoder/sink prepared in {:.1} ms (device: {})",
                id,
                prepare_started.elapsed().as_secs_f64() * 1000.0,
                device_name
            );

            players.push(ActivePlayer {
                id,
                generation,
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
                volume_automation_enabled,
                volume_automation,
                time_pitch: time_pitch_state,
                position_state,
                prefetch_underrun_count,
                last_prefetch_underrun_count: 0,
                prefetch_underrun_frame_count,
                last_prefetch_underrun_frame_count: 0,
                automation_update_tx,
                automation_update_rx,
                fade_in_secs: effective_fade_in_secs,
                fade_out_secs,
                fade_curve,
                custom_fade_points,
                channel_routes: route_state,
                output_gains: output_gains_state,
                output_channels,
                output_sample_rate: sample_rate,
                output_device: device_name,
                uses_global_default,
                mixer,
                output_settings,
                loop_enabled,
                loop_runtime_active,
                loop_start,
                loop_end: effective_loop_end,
                loops_remaining: loops_remaining_state,
                loop_iterations,
                last_reported_loop_iterations: 0,
                render_loop_iterations,
                last_reported_render_loop_iterations: 0,
                render_loop_boundary_frame,
                render_loop_boundary_position_bits,
                render_loop_boundary_endpoint_frame,
                render_loop_fingerprint,
                render_loop_fingerprint_iteration,
                last_reported_render_loop_fingerprint_iteration: 0,
                loop_pcm_capture,
                last_reported_loop_pcm_capture_iteration: 0,
                playback_elapsed_frames,
                render_loop_remaining_state,
                source_sample_rate,
                paused_at: None,
                dynamic_fade: None,
                stop_fade,
                stopping: false,
                pending_stop_completions: Vec::new(),
                start_frame_observed,
                start_frame_reported: false,
                launch_transaction_id,
            });
        }

        AudioCommand::Stop(id) => {
            for player in players.iter_mut().filter(|player| player.id == id) {
                if player.stopping {
                    continue;
                }
                let _ = pos_tx.try_send(PositionUpdate {
                    id: player.id,
                    generation: player.generation,
                    position_secs: player.current_pos(),
                    state: CueState::Stopped,
                });
                player.stopping = true;
                if player_is_paused(streams, player) {
                    // A paused source cannot render a fade. It is already silent
                    // at the source, so complete the stop without resuming audio.
                    player.stop_fade.complete_without_render();
                    player.sink.stop();
                } else {
                    player.stop_fade.request();
                }
            }
        }

        AudioCommand::StopTimed {
            id,
            request_id,
            requested_at_unix_micros,
        } => {
            let elapsed_us = unix_micros_now().saturating_sub(requested_at_unix_micros);
            let requested_at = Instant::now();
            let mut matching_players = 0usize;
            for player in players.iter_mut().filter(|player| player.id == id) {
                matching_players += 1;
                if !player.stopping {
                    let _ = pos_tx.try_send(PositionUpdate {
                        id: player.id,
                        generation: player.generation,
                        position_secs: player.current_pos(),
                        state: CueState::Stopped,
                    });
                    player.stopping = true;
                    if player_is_paused(streams, player) {
                        player.stop_fade.complete_without_render();
                        player.sink.stop();
                    } else {
                        player.stop_fade.request();
                    }
                }
                player.pending_stop_completions.push(PendingStopCompletion {
                    request_id: request_id.clone(),
                    cue_id: Some(id),
                    requested_at: requested_at.clone(),
                });
            }
            info!(
                "STOP_LATENCY: request_id={}, cue_id={}, stage=stop_fade_requested, request_to_engine_us={}, matching_players={}, fade_ms={:.2}",
                request_id, id, elapsed_us, matching_players, STOP_DECLICK_FADE_MS
            );
        }

        AudioCommand::StopAllTimed {
            request_id,
            requested_at_unix_micros,
        } => {
            let elapsed_us = unix_micros_now().saturating_sub(requested_at_unix_micros);
            let requested_at = Instant::now();
            let stopped = players.len();
            for player in players.iter_mut() {
                if !player.stopping {
                    let _ = pos_tx.try_send(PositionUpdate {
                        id: player.id,
                        generation: player.generation,
                        position_secs: player.current_pos(),
                        state: CueState::Stopped,
                    });
                    player.stopping = true;
                    if player_is_paused(streams, player) {
                        player.stop_fade.complete_without_render();
                        player.sink.stop();
                    } else {
                        player.stop_fade.request();
                    }
                }
                player.pending_stop_completions.push(PendingStopCompletion {
                    request_id: request_id.clone(),
                    cue_id: None,
                    requested_at: requested_at.clone(),
                });
            }
            info!(
                "STOP_LATENCY: request_id={}, scope=all, stage=stop_fade_requested, request_to_engine_us={}, matching_players={}, fade_ms={:.2}",
                request_id, elapsed_us, stopped, STOP_DECLICK_FADE_MS
            );
        }

        AudioCommand::StopAll => {
            for player in players.iter_mut() {
                if player.stopping {
                    continue;
                }
                let _ = pos_tx.try_send(PositionUpdate {
                    id: player.id,
                    generation: player.generation,
                    position_secs: player.current_pos(),
                    state: CueState::Stopped,
                });
                player.stopping = true;
                if player_is_paused(streams, player) {
                    player.stop_fade.complete_without_render();
                    player.sink.stop();
                } else {
                    player.stop_fade.request();
                }
            }
        }

        AudioCommand::Pause(id) => {
            if let Some(player) = players
                .iter_mut()
                .rev()
                .find(|player| player.id == id && !player.stopping)
            {
                let position = player.current_pos();
                player.sink.pause();
                player.paused_at = Some(position);
            }
        }

        AudioCommand::Seek { id, position_secs } => {
            if let Some(player) = players
                .iter_mut()
                .rev()
                .find(|player| player.id == id && !player.stopping)
            {
                let min_position = player.start_secs.max(0.0);
                let max_position = if player.end_secs > 0.0 {
                    (player.end_secs - 0.01).max(min_position)
                } else {
                    f64::MAX
                };
                let position = position_secs.clamp(min_position, max_position);
                player.sink.stop();
                let endpoint_frame_clock = streams
                    .get(&player.output_device)
                    .map(|output| output.sample_frame_clock.clone());
                let channel_router_profile = streams
                    .get(&player.output_device)
                    .and_then(|output| output.stream_health.clone());
                let mut pump_during_prefetch = || {
                    #[cfg(target_os = "windows")]
                    pump_directsound_queues(streams);
                };
                let stop_fade = StopFadeControl::new();
                // Use the loop count at the audible render position, not the
                // producer's read-ahead count, which can be up to the prefetch
                // queue ahead of what the operator has actually heard.
                let loops_remaining_for_seek = Arc::new(AtomicI32::new(
                    player.render_loop_remaining_state.load(Ordering::Acquire),
                ));
                let new_sink_result = open_sink_at(
                    &player.mixer,
                    &player.file,
                    position,
                    player.current_volume,
                    player.loop_enabled,
                    player.loop_start,
                    player.loop_end,
                    loops_remaining_for_seek.clone(),
                    player.loop_iterations.clone(),
                    player.playback_elapsed_frames.clone(),
                    player.render_loop_remaining_state.clone(),
                    stop_fade.clone(),
                    player.output_channels,
                    player.output_sample_rate,
                    player.channel_routes.clone(),
                    player.output_settings.clone(),
                    player.output_gains.clone(),
                    channel_router_profile,
                    player.volume_automation_enabled,
                    player.volume_automation.clone(),
                    player.automation_update_rx.clone(),
                    retired_automation_points_tx.clone(),
                    player.time_pitch.clone(),
                    player.position_state.clone(),
                    player.prefetch_underrun_count.clone(),
                    player.prefetch_underrun_frame_count.clone(),
                    player.render_loop_iterations.clone(),
                    player.render_loop_boundary_frame.clone(),
                    player.render_loop_boundary_position_bits.clone(),
                    player.render_loop_boundary_endpoint_frame.clone(),
                    player.loop_pcm_capture.clone(),
                    player.render_loop_fingerprint.clone(),
                    player.render_loop_fingerprint_iteration.clone(),
                    &mut pump_during_prefetch,
                    None,
                    endpoint_frame_clock,
                    None,
                );
                drop(pump_during_prefetch);
                if let Some((new_sink, source_sample_rate)) = new_sink_result {
                    new_sink.pause();
                    player.sink = new_sink;
                    player.loops_remaining = loops_remaining_for_seek;
                    player.source_sample_rate = source_sample_rate;
                    player.stop_fade = stop_fade;
                    player.stopping = false;
                    player.pending_stop_completions.clear();
                    player.paused_at = Some(position);
                    player.dynamic_fade = None;
                } else {
                    warn!("Could not seek audio cue {} to {:.3}s", id, position);
                }
            }
        }

        AudioCommand::PauseAll => {
            // CPAL endpoints pause the complete final mix at one shared sample
            // boundary. Do not call Sink::pause() sequentially for those Cue:
            // the native callback could run between calls and create phase drift.
            for (device_name, output) in streams.iter_mut() {
                if let Some(gate) = &output.transport_paused {
                    let frame = output.sample_frame_clock.load(Ordering::Acquire);
                    gate.store(true, Ordering::Release);
                    info!(
                        "AUDIO_TRANSPORT_PAUSE_ALL: endpoint='{}', endpoint_frame={}, active_cues={}",
                        device_name,
                        frame,
                        players.iter().filter(|player| player.output_device.as_str() == device_name.as_str() && !player.stopping).count()
                    );
                }
            }

            // DirectSound uses a queued-output path without a CPAL callback
            // gate. Keep its existing per-Sink pause behavior for now.
            for player in players.iter_mut().filter(|player| !player.stopping) {
                if player_output_is_globally_paused(streams, player) {
                    continue;
                }
                let position = player.current_pos();
                player.sink.pause();
                player.paused_at = Some(position);
            }
        }

        AudioCommand::Resume(id) => {
            let gated_device = players
                .iter()
                .rev()
                .find(|player| player.id == id && !player.stopping)
                .and_then(|player| {
                    let output = streams.get(&player.output_device)?;
                    let gate = output.transport_paused.as_ref()?;
                    gate.load(Ordering::Acquire).then(|| {
                        (
                            player.output_device.clone(),
                            player.generation,
                            gate.clone(),
                        )
                    })
                });

            if let Some((device_name, selected_generation, gate)) = gated_device {
                // Resume Selected while Pause All is active: keep the endpoint
                // gate closed while individually pausing the other Cue on this
                // endpoint, then release the gate once the selected Cue is ready.
                for other in players.iter_mut().filter(|player| {
                    player.output_device.as_str() == device_name.as_str()
                        && !(player.id == id && player.generation == selected_generation)
                        && !player.stopping
                }) {
                    if !other.sink.is_paused() {
                        let position = other.current_pos();
                        other.sink.pause();
                        other.paused_at = Some(position);
                    }
                }

                if let Some(player) = players
                    .iter_mut()
                    .rev()
                    .find(|player| player.id == id && !player.stopping)
                {
                    player.paused_at = None;
                    player.sink.play();
                }

                gate.store(false, Ordering::Release);
            } else if let Some(player) = players
                .iter_mut()
                .rev()
                .find(|player| player.id == id && !player.stopping)
            {
                player.paused_at = None;
                player.sink.play();
            }
        }

        AudioCommand::ResumeAll => {
            // Prepare every Sink while the shared endpoint gates are still
            // closed. Releasing each gate only after all Sinks are ready makes
            // the next output sample the common resume point for that endpoint.
            for player in players.iter_mut().filter(|player| !player.stopping) {
                player.paused_at = None;
                player.sink.play();
            }
            for (device_name, output) in streams.iter_mut() {
                if let Some(gate) = &output.transport_paused {
                    let frame = output.sample_frame_clock.load(Ordering::Acquire);
                    gate.store(false, Ordering::Release);
                    info!(
                        "AUDIO_TRANSPORT_RESUME_ALL: endpoint='{}', endpoint_frame={}, active_cues={}",
                        device_name,
                        frame,
                        players.iter().filter(|player| player.output_device.as_str() == device_name.as_str() && !player.stopping).count()
                    );
                }
            }
        }

        AudioCommand::SetVolume(id, volume) => {
            if let Some(player) = players
                .iter_mut()
                .rev()
                .find(|player| player.id == id && !player.stopping)
            {
                player.base_volume = volume;
                player.sink.set_volume(volume);
            }
        }

        AudioCommand::SetVolumeAutomation {
            id,
            enabled,
            points,
        } => {
            if let Some(player) = players
                .iter_mut()
                .rev()
                .find(|player| player.id == id && !player.stopping)
            {
                player.volume_automation_enabled = enabled;
                player.volume_automation = points.clone();
                // The source consumes updates at audio-frame boundaries without blocking.
                let _ = player
                    .automation_update_tx
                    .send(VolumeAutomationUpdate { enabled, points });
            }
        }
        AudioCommand::SetTimePitch { id, mut settings } => {
            sanitize_time_pitch_settings(&mut settings);
            if let Some(player) = players
                .iter_mut()
                .rev()
                .find(|player| player.id == id && !player.stopping)
            {
                *player.time_pitch.write() = settings;
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
            if let Some(player) = players
                .iter_mut()
                .rev()
                .find(|player| player.id == id && !player.stopping)
            {
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
            if let Some(player) = players
                .iter_mut()
                .rev()
                .find(|player| player.id == id && !player.stopping)
            {
                *player.channel_routes.write() = routes;
            }
        }

        AudioCommand::SetOutputGains { id, gains_db } => {
            if let Some(player) = players
                .iter_mut()
                .rev()
                .find(|player| player.id == id && !player.stopping)
            {
                let mut current = player.output_gains.write();
                *current = gains_db;
                current.resize(player.output_channels, 0.0);
                current.truncate(player.output_channels);
                for gain in current.iter_mut() {
                    if !gain.is_finite() {
                        *gain = 0.0;
                    }
                    *gain = (*gain).clamp(-60.0, 18.0);
                }
            }
        }

        AudioCommand::TestOutput {
            device_name,
            logical_channel,
            channel,
        } => {
            // A new test smoothly cancels the previous test source instead of
            // abruptly stopping a Player (which can create a click/pop).
            let generation = test_generation.fetch_add(1, Ordering::Relaxed) + 1;

            // Use the exact active worker stream selected in Settings. The source
            // is added directly to that stream's mixer and emits non-zero samples
            // only on the requested native interleaved channel.
            let Some((mixer, output_channels, sample_rate, _)) = ensure_device_output(
                backend,
                streams,
                &device_name,
                context,
                output_settings_by_device,
                output_peak_meter,
                performance,
            ) else {
                warn!(
                    "Could not open output device '{}' for channel test",
                    device_name
                );
                return;
            };

            if channel >= output_channels {
                warn!(
                    "Test output channel {} is outside the active {}-channel stream for '{}'",
                    channel + 1,
                    output_channels,
                    device_name
                );
                return;
            }

            let duration_frames = (sample_rate as u64 * 900 / 1_000).max(1);
            let fade_frames = (sample_rate as u64 * 8 / 1_000).max(1);
            let source = RawOutputTestTone {
                output_channels,
                selected_channel: channel,
                sample_rate,
                sample_index: 0,
                duration_frames,
                fade_frames,
                generation,
                current_generation: test_generation.clone(),
                stop_frame: None,
            };

            mixer.add(source);
            info!(
                "OUTPUT TEST: backend='{}', device='{}', generation={}, logical output {} -> native stream channel {} (zero-based index {}), stream={}ch @ {}Hz, duration=900ms, fade=8ms; direct mixer source; only selected interleaved channel is non-zero",
                backend.short_label(),
                device_name,
                generation,
                logical_channel + 1,
                channel + 1,
                channel,
                output_channels,
                sample_rate
            );
        }
        AudioCommand::SetOutputChannelSettings {
            device_name,
            settings,
        } => {
            let channels = streams
                .get(&device_name)
                .map(|output| output.channels)
                .unwrap_or_else(|| settings.len().max(1));
            let shared = shared_output_settings(output_settings_by_device, &device_name, channels);
            let mut current = shared.write();
            *current = settings;
            current.resize(channels.max(1), OutputChannelSetting::default());
            current.truncate(channels.max(1));
        }
        AudioCommand::PlayBatch { .. } => {
            warn!("PlayBatch reached the single-command handler; refusing to bypass sample scheduling");
        }
        AudioCommand::BeginLaunchTransaction { .. }
        | AudioCommand::EndLaunchTransaction { .. }
        | AudioCommand::SetOutputDevice { .. }
        | AudioCommand::ReconfigureOutput { .. }
        | AudioCommand::SetEndpointPreferences { .. }
        | AudioCommand::UpdateDeviceRegistry { .. }
        | AudioCommand::SetInputDevice(_)
        | AudioCommand::Shutdown => {}
    }
}

#[cfg(test)]
mod channel_router_tests {
    use super::*;
    use rodio::buffer::SamplesBuffer;

    #[test]
    fn stop_fade_source_ramps_interleaved_audio_to_zero_without_channel_skew() {
        use std::num::{NonZeroU16, NonZeroU32};

        let channels = 2_u16;
        let sample_rate = 48_000_u32;
        let fade_frames =
            ((sample_rate as f64 * STOP_DECLICK_FADE_MS / 1_000.0).round() as usize).max(1);
        let source = SamplesBuffer::new(
            NonZeroU16::new(channels).unwrap(),
            NonZeroU32::new(sample_rate).unwrap(),
            vec![0.75_f32; (fade_frames + 8) * channels as usize],
        );
        let control = StopFadeControl::new();
        let mut faded = StopFadeSource::new(source, control.clone());

        // Let one complete frame pass before requesting Stop.
        assert_eq!(faded.next(), Some(0.75));
        assert_eq!(faded.next(), Some(0.75));
        control.request();

        let tail = faded.by_ref().collect::<Vec<_>>();
        assert_eq!(tail.len(), fade_frames * channels as usize);
        assert_eq!(tail[0], 0.75);
        assert_eq!(tail[1], 0.75);
        assert_eq!(tail[tail.len() - 2], 0.0);
        assert_eq!(tail[tail.len() - 1], 0.0);

        let mut previous_gain = 1.0_f32;
        for frame in 0..fade_frames {
            let left = tail[frame * channels as usize];
            let right = tail[frame * channels as usize + 1];
            assert!((left - right).abs() <= f32::EPSILON);
            let gain = left / 0.75;
            assert!(
                gain <= previous_gain + 1.0e-6,
                "stop fade gain must not increase: previous={previous_gain}, current={gain}"
            );
            previous_gain = gain;
        }

        assert!(control.is_completed());
        assert_eq!(faded.next(), None);
    }

    #[test]
    fn stop_fade_request_mid_frame_waits_for_next_interleaved_frame() {
        use std::num::{NonZeroU16, NonZeroU32};

        let source = SamplesBuffer::new(
            NonZeroU16::new(2).unwrap(),
            NonZeroU32::new(48_000).unwrap(),
            vec![0.5_f32; 2_000],
        );
        let control = StopFadeControl::new();
        let mut faded = StopFadeSource::new(source, control.clone());

        // Request Stop between the left and right samples of a stereo frame.
        assert_eq!(faded.next(), Some(0.5));
        control.request();
        assert_eq!(faded.next(), Some(0.5));

        let tail = faded.by_ref().collect::<Vec<_>>();
        assert_eq!(tail.len() % 2, 0);
        assert_eq!(tail[0], 0.5);
        assert_eq!(tail[1], 0.5);
        for frame in 0..(tail.len() / 2) {
            assert!((tail[frame * 2] - tail[frame * 2 + 1]).abs() <= f32::EPSILON);
        }
        assert_eq!(tail[tail.len() - 2], 0.0);
        assert_eq!(tail[tail.len() - 1], 0.0);
        assert!(control.is_completed());
    }

    #[test]
    fn stop_fade_completion_mid_wide_frame_waits_for_next_interleaved_frame() {
        use std::num::{NonZeroU16, NonZeroU32};

        let channels = 128_usize;
        let expected = (0..channels)
            .map(|index| (index + 1) as f32 / channels as f32)
            .collect::<Vec<_>>();
        let source = SamplesBuffer::new(
            NonZeroU16::new(channels as u16).unwrap(),
            NonZeroU32::new(48_000).unwrap(),
            expected.clone(),
        );
        let control = StopFadeControl::new();
        let mut faded = StopFadeSource::new(source, control.clone());

        // Completing between two samples must not truncate the current
        // 128-channel frame. The source ends at the next frame boundary.
        assert_eq!(faded.next(), Some(expected[0]));
        control.complete_without_render();
        for expected_sample in expected.iter().skip(1) {
            assert_eq!(faded.next(), Some(*expected_sample));
        }
        assert_eq!(faded.next(), None);
    }

    #[test]
    fn stop_fade_source_passes_audio_unchanged_until_stop_is_requested() {
        use std::num::{NonZeroU16, NonZeroU32};

        let expected = vec![0.1_f32, -0.1, 0.2, -0.2];
        let source = SamplesBuffer::new(
            NonZeroU16::new(2).unwrap(),
            NonZeroU32::new(48_000).unwrap(),
            expected.clone(),
        );
        let control = StopFadeControl::new();
        let actual = StopFadeSource::new(source, control.clone()).collect::<Vec<_>>();

        assert_eq!(actual, expected);
        assert!(!control.is_completed());
    }

    #[test]
    fn pcm_prefetch_source_preserves_interleaved_samples_and_reaches_eof() {
        use std::num::{NonZeroU16, NonZeroU32};

        let expected = vec![0.1_f32, -0.1, 0.2, -0.2, 0.3, -0.3, 0.4, -0.4];
        let source = SamplesBuffer::new(
            NonZeroU16::new(2).unwrap(),
            NonZeroU32::new(48_000).unwrap(),
            expected.clone(),
        );
        let producer_position = Arc::new(AtomicU64::new(0.0_f64.to_bits()));
        let render_position = Arc::new(AtomicU64::new(0.0_f64.to_bits()));
        let underruns = Arc::new(AtomicU64::new(0));
        let playback_elapsed_frames = Arc::new(AtomicU64::new(0));
        let mut pump_during_wait = || {};
        let mut prefetched = PcmPrefetchSource::new(
            source,
            producer_position,
            render_position,
            underruns.clone(),
            playback_elapsed_frames.clone(),
            Arc::new(AtomicI32::new(0)),
            Arc::new(AtomicI32::new(0)),
            Arc::new(AtomicU64::new(0)),
            false,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0.0_f64.to_bits())),
            Arc::new(AtomicU64::new(u64::MAX)),
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(LoopPcmCapture::new()),
            &mut pump_during_wait,
        )
        .expect("prefetch source should prime its first chunk");

        let actual = (0..expected.len())
            .map(|_| prefetched.next().expect("prefetched sample"))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert_eq!(prefetched.next(), None);
        assert_eq!(underruns.load(Ordering::Relaxed), 0);
        assert_eq!(playback_elapsed_frames.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn pcm_prefetch_underrun_finishes_the_interleaved_frame_before_resuming_pcm() {
        use std::num::{NonZeroU16, NonZeroU32};

        let (ready_tx, ready_rx) = bounded::<PcmPrefetchChunk>(2);
        let (free_tx, _free_rx) = bounded::<PcmPrefetchChunk>(2);
        let underruns = Arc::new(AtomicU64::new(0));
        let underrun_frames = Arc::new(AtomicU64::new(0));
        let playback_elapsed_frames = Arc::new(AtomicU64::new(0));
        let mut source = PcmPrefetchSource {
            channels: NonZeroU16::new(2).unwrap(),
            sample_rate: NonZeroU32::new(48_000).unwrap(),
            total_duration: None,
            ready_rx,
            free_tx,
            current_chunk: None,
            current_index: 0,
            producer_finished: Arc::new(AtomicBool::new(false)),
            render_position_state: Arc::new(AtomicU64::new(0.0_f64.to_bits())),
            underrun_count: underruns.clone(),
            playback_elapsed_frames: playback_elapsed_frames.clone(),
            render_loop_remaining_state: Arc::new(AtomicI32::new(-1)),
            underrun_frame_count: underrun_frames.clone(),
            underrun_silence_samples_remaining: 0,
            underrun_active: false,
            loop_diagnostics_enabled: false,
            render_loop_iterations: Arc::new(AtomicU64::new(0)),
            render_loop_boundary_frame: Arc::new(AtomicU64::new(0)),
            render_loop_boundary_position_bits: Arc::new(AtomicU64::new(0.0_f64.to_bits())),
            render_loop_boundary_endpoint_frame: Arc::new(AtomicU64::new(u64::MAX)),
            endpoint_frame_clock: None,
            render_loop_fingerprint: Arc::new(AtomicU64::new(0)),
            render_loop_fingerprint_iteration: Arc::new(AtomicU64::new(0)),
            loop_pcm_capture: Arc::new(LoopPcmCapture::new()),
            loop_capture_pre_samples: [0.0; PCM_LOOP_CAPTURE_PRE_FRAMES
                * PCM_LOOP_CAPTURE_MAX_CHANNELS],
            loop_capture_pre_frames: 0,
            loop_capture_pre_write_frame: 0,
            loop_capture_active: false,
            loop_capture_iteration: 0,
            loop_capture_post_samples_written: 0,
            last_render_media_position: None,
            loop_fingerprint_active: false,
            loop_fingerprint_samples_remaining: 0,
            loop_fingerprint_hash: PCM_FNV1A64_OFFSET_BASIS,
            loop_fingerprint_iteration: 0,
            consumer_finished: false,
        };

        // The first channel is missing. A chunk becomes ready before the second
        // channel is requested, but the remainder of this frame must stay silent.
        assert_eq!(source.next(), Some(0.0));
        assert!(ready_tx
            .send(PcmPrefetchChunk {
                samples: vec![0.25, -0.25],
                frame_end_positions: vec![1.0_f64.to_bits()],
                frame_loop_remaining: vec![-1],
                start_position_bits: 0.0_f64.to_bits(),
                end_position_bits: 1.0_f64.to_bits(),
                start_loop_remaining: -1,
                eof: true,
            })
            .is_ok());

        assert_eq!(source.next(), Some(0.0));
        assert_eq!(source.next(), Some(0.25));
        assert_eq!(source.next(), Some(-0.25));
        assert_eq!(underruns.load(Ordering::Relaxed), 1);
        assert_eq!(underrun_frames.load(Ordering::Relaxed), 1);
        assert_eq!(playback_elapsed_frames.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn pcm_prefetch_reports_loop_boundary_when_render_reaches_wrapped_pcm() {
        use std::num::{NonZeroU16, NonZeroU32};

        let (ready_tx, ready_rx) = bounded::<PcmPrefetchChunk>(2);
        let (free_tx, _free_rx) = bounded::<PcmPrefetchChunk>(2);
        assert!(ready_tx
            .send(PcmPrefetchChunk {
                samples: vec![0.1, -0.1],
                frame_end_positions: vec![10.0_f64.to_bits()],
                frame_loop_remaining: vec![-1],
                start_position_bits: 9.9_f64.to_bits(),
                end_position_bits: 10.0_f64.to_bits(),
                start_loop_remaining: -1,
                eof: false,
            })
            .is_ok());
        let loop_samples = (0..PCM_LOOP_FINGERPRINT_FRAMES)
            .flat_map(|_| [0.2_f32, -0.2_f32])
            .collect::<Vec<_>>();
        let loop_frame_end_positions = (1..=PCM_LOOP_FINGERPRINT_FRAMES)
            .map(|frame| (5.0_f64 + frame as f64 / 48_000.0).to_bits())
            .collect::<Vec<_>>();
        assert!(ready_tx
            .send(PcmPrefetchChunk {
                samples: loop_samples.clone(),
                frame_end_positions: loop_frame_end_positions,
                frame_loop_remaining: vec![-1; PCM_LOOP_FINGERPRINT_FRAMES],
                start_position_bits: 5.0_f64.to_bits(),
                end_position_bits: (5.0_f64 + PCM_LOOP_FINGERPRINT_FRAMES as f64 / 48_000.0)
                    .to_bits(),
                start_loop_remaining: -1,
                eof: false,
            })
            .is_ok());

        let playback_elapsed_frames = Arc::new(AtomicU64::new(0));
        let render_loop_iterations = Arc::new(AtomicU64::new(0));
        let render_loop_boundary_frame = Arc::new(AtomicU64::new(0));
        let render_loop_boundary_position_bits = Arc::new(AtomicU64::new(0.0_f64.to_bits()));
        let render_loop_boundary_endpoint_frame = Arc::new(AtomicU64::new(u64::MAX));
        let endpoint_frame_clock = Arc::new(AtomicU64::new(42));
        let loop_pcm_capture = Arc::new(LoopPcmCapture::new());
        let render_loop_fingerprint = Arc::new(AtomicU64::new(0));
        let render_loop_fingerprint_iteration = Arc::new(AtomicU64::new(0));
        let mut source = PcmPrefetchSource {
            channels: NonZeroU16::new(2).unwrap(),
            sample_rate: NonZeroU32::new(48_000).unwrap(),
            total_duration: None,
            ready_rx,
            free_tx,
            current_chunk: None,
            current_index: 0,
            producer_finished: Arc::new(AtomicBool::new(true)),
            render_position_state: Arc::new(AtomicU64::new(0.0_f64.to_bits())),
            underrun_count: Arc::new(AtomicU64::new(0)),
            playback_elapsed_frames: playback_elapsed_frames.clone(),
            render_loop_remaining_state: Arc::new(AtomicI32::new(-1)),
            underrun_frame_count: Arc::new(AtomicU64::new(0)),
            underrun_silence_samples_remaining: 0,
            underrun_active: false,
            loop_diagnostics_enabled: true,
            render_loop_iterations: render_loop_iterations.clone(),
            render_loop_boundary_frame: render_loop_boundary_frame.clone(),
            render_loop_boundary_position_bits: render_loop_boundary_position_bits.clone(),
            render_loop_boundary_endpoint_frame: render_loop_boundary_endpoint_frame.clone(),
            endpoint_frame_clock: Some(endpoint_frame_clock.clone()),
            render_loop_fingerprint: render_loop_fingerprint.clone(),
            render_loop_fingerprint_iteration: render_loop_fingerprint_iteration.clone(),
            loop_pcm_capture: loop_pcm_capture.clone(),
            loop_capture_pre_samples: [0.0; PCM_LOOP_CAPTURE_PRE_FRAMES
                * PCM_LOOP_CAPTURE_MAX_CHANNELS],
            loop_capture_pre_frames: 0,
            loop_capture_pre_write_frame: 0,
            loop_capture_active: false,
            loop_capture_iteration: 0,
            loop_capture_post_samples_written: 0,
            last_render_media_position: None,
            loop_fingerprint_active: false,
            loop_fingerprint_samples_remaining: 0,
            loop_fingerprint_hash: PCM_FNV1A64_OFFSET_BASIS,
            loop_fingerprint_iteration: 0,
            consumer_finished: false,
        };

        assert_eq!(source.next(), Some(0.1));
        assert_eq!(source.next(), Some(-0.1));
        assert_eq!(playback_elapsed_frames.load(Ordering::Relaxed), 1);
        assert_eq!(render_loop_iterations.load(Ordering::Acquire), 0);

        // The second chunk starts at 5.0s after the previous frame ended at 10.0s.
        assert_eq!(source.next(), Some(0.2));
        assert_eq!(render_loop_iterations.load(Ordering::Acquire), 1);
        assert_eq!(render_loop_boundary_frame.load(Ordering::Acquire), 1);
        assert_eq!(
            render_loop_boundary_endpoint_frame.load(Ordering::Acquire),
            42
        );
        assert!(
            (f64::from_bits(render_loop_boundary_position_bits.load(Ordering::Acquire)) - 5.0)
                .abs()
                < f64::EPSILON
        );
        for expected in loop_samples.iter().skip(1) {
            assert_eq!(source.next(), Some(*expected));
        }
        let expected_fingerprint = loop_samples
            .iter()
            .fold(PCM_FNV1A64_OFFSET_BASIS, |hash, sample| {
                (hash ^ sample.to_bits() as u64).wrapping_mul(PCM_FNV1A64_PRIME)
            });
        assert_eq!(render_loop_fingerprint_iteration.load(Ordering::Acquire), 1);
        assert_eq!(
            render_loop_fingerprint.load(Ordering::Acquire),
            expected_fingerprint
        );

        assert_eq!(
            loop_pcm_capture.completed_iteration.load(Ordering::Acquire),
            1
        );
        assert_eq!(
            loop_pcm_capture.status.load(Ordering::Relaxed),
            PCM_LOOP_CAPTURE_STATUS_VALID
        );
        assert_eq!(loop_pcm_capture.channels.load(Ordering::Relaxed), 2);
        assert_eq!(loop_pcm_capture.pre_frames_valid.load(Ordering::Relaxed), 1);
        assert_eq!(
            loop_pcm_capture
                .post_samples_captured
                .load(Ordering::Relaxed),
            (PCM_LOOP_CAPTURE_POST_FRAMES * 2) as u32
        );
        assert_eq!(
            loop_pcm_capture
                .source_elapsed_frame
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            loop_pcm_capture.source_media_frame.load(Ordering::Relaxed),
            240_000
        );
        assert_eq!(
            loop_pcm_capture
                .endpoint_render_frame
                .load(Ordering::Relaxed),
            42
        );
        // The one available pre-boundary frame is right-aligned in the
        // four-frame pre-window; the post-window begins at a fixed offset.
        assert_eq!(
            loop_pcm_capture.samples[6].load(Ordering::Relaxed),
            0.1_f32.to_bits()
        );
        assert_eq!(
            loop_pcm_capture.samples[7].load(Ordering::Relaxed),
            (-0.1_f32).to_bits()
        );
        for (index, expected) in loop_samples
            .iter()
            .take(PCM_LOOP_CAPTURE_POST_FRAMES * 2)
            .enumerate()
        {
            assert_eq!(
                loop_pcm_capture.samples[PCM_LOOP_CAPTURE_PRE_FRAMES * 2 + index]
                    .load(Ordering::Relaxed),
                expected.to_bits()
            );
        }
    }

    #[test]
    fn sample_frame_clock_advances_once_per_complete_interleaved_frame() {
        use std::num::{NonZeroU16, NonZeroU32};

        let channels = 6_usize;
        let source = SamplesBuffer::new(
            NonZeroU16::new(channels as u16).unwrap(),
            NonZeroU32::new(48_000).unwrap(),
            vec![0.1_f32; 24],
        );
        let frame_clock = Arc::new(AtomicU64::new(0));
        let mut clocked = SampleFrameClockSource::new(source, frame_clock.clone(), channels);

        for _ in 0..(channels - 1) {
            assert!(clocked.next().is_some());
            assert_eq!(frame_clock.load(Ordering::Relaxed), 0);
        }
        assert!(clocked.next().is_some());
        assert_eq!(frame_clock.load(Ordering::Relaxed), 1);
        for _ in 0..channels {
            assert!(clocked.next().is_some());
        }
        assert_eq!(frame_clock.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn output_transport_gate_freezes_all_sources_without_stopping_output_clock() {
        use std::num::{NonZeroU16, NonZeroU32};

        let source = SamplesBuffer::new(
            NonZeroU16::new(2).unwrap(),
            NonZeroU32::new(48_000).unwrap(),
            vec![0.25_f32, 0.5, 0.75, 1.0],
        );
        let paused = Arc::new(AtomicBool::new(true));
        let gated = OutputTransportGateSource::new(source, paused.clone());
        let frame_clock = Arc::new(AtomicU64::new(0));
        let mut output = SampleFrameClockSource::new(gated, frame_clock.clone(), 2);

        // While paused, the endpoint emits silence and advances its output
        // frame clock, but the underlying mixer/source is not polled.
        assert_eq!(output.next(), Some(0.0));
        assert_eq!(frame_clock.load(Ordering::Acquire), 0);
        assert_eq!(output.next(), Some(0.0));
        assert_eq!(frame_clock.load(Ordering::Acquire), 1);
        assert_eq!(output.next(), Some(0.0));
        assert_eq!(frame_clock.load(Ordering::Acquire), 1);
        assert_eq!(output.next(), Some(0.0));
        assert_eq!(frame_clock.load(Ordering::Acquire), 2);

        // Repeated pause/resume cycles do not consume source samples. Once the
        // shared gate opens, playback resumes at the exact original sample.
        for _ in 0..100 {
            paused.store(true, Ordering::Release);
            assert_eq!(output.next(), Some(0.0));
            assert_eq!(output.next(), Some(0.0));
            paused.store(false, Ordering::Release);
        }
        assert_eq!(output.next(), Some(0.25));
        assert_eq!(output.next(), Some(0.5));
        assert_eq!(frame_clock.load(Ordering::Acquire), 103);
    }

    #[test]
    fn output_transport_gate_never_splits_an_interleaved_frame() {
        use std::num::{NonZeroU16, NonZeroU32};

        let source = SamplesBuffer::new(
            NonZeroU16::new(2).unwrap(),
            NonZeroU32::new(48_000).unwrap(),
            vec![0.25_f32, 0.5, 0.75, 1.0],
        );
        let paused = Arc::new(AtomicBool::new(false));
        let mut gated = OutputTransportGateSource::new(source, paused.clone());

        // A pause arriving after the first channel takes effect only after
        // the second channel of the same interleaved frame has been delivered.
        assert_eq!(gated.next(), Some(0.25));
        paused.store(true, Ordering::Release);
        assert_eq!(gated.next(), Some(0.5));

        // The next complete frame is silent. A resume between its channels
        // cannot leak one channel early.
        assert_eq!(gated.next(), Some(0.0));
        paused.store(false, Ordering::Release);
        assert_eq!(gated.next(), Some(0.0));

        // The following frame resumes from the next untouched source frame.
        assert_eq!(gated.next(), Some(0.75));
        assert_eq!(gated.next(), Some(1.0));
    }

    #[test]
    fn scheduled_source_waits_for_the_shared_target_sample_frame() {
        use std::num::{NonZeroU16, NonZeroU32};

        let source = SamplesBuffer::new(
            NonZeroU16::new(2).unwrap(),
            NonZeroU32::new(48_000).unwrap(),
            vec![0.25_f32, 0.5, 0.75, 1.0],
        );
        let frame_clock = Arc::new(AtomicU64::new(10));
        let start_gate = Arc::new(AtomicU64::new(12));
        let observed_start = Arc::new(AtomicU64::new(u64::MAX));
        let mut scheduled = SampleScheduledSource::new(
            source,
            frame_clock.clone(),
            start_gate,
            Some(observed_start.clone()),
        );

        // Both channels of frames 10 and 11 remain silent.
        assert_eq!(scheduled.next(), Some(0.0));
        assert_eq!(scheduled.next(), Some(0.0));
        assert_eq!(scheduled.next(), Some(0.0));
        assert_eq!(scheduled.next(), Some(0.0));

        frame_clock.store(12, Ordering::Relaxed);
        assert_eq!(scheduled.next(), Some(0.25));
        assert_eq!(scheduled.next(), Some(0.5));
        assert_eq!(scheduled.next(), Some(0.75));
        assert_eq!(scheduled.next(), Some(1.0));
        assert_eq!(observed_start.load(Ordering::Acquire), 12);
    }

    #[test]
    fn mixer_batch_sources_begin_on_the_same_output_frame() {
        use std::num::{NonZeroU16, NonZeroU32};

        let channels = 2_u16;
        let sample_rate = 48_000_u32;
        let (mixer, output) = rodio::mixer::mixer(
            NonZeroU16::new(channels).unwrap(),
            NonZeroU32::new(sample_rate).unwrap(),
        );
        let frame_clock = Arc::new(AtomicU64::new(0));
        let start_gate = Arc::new(AtomicU64::new(3));

        let observed_frames = [
            Arc::new(AtomicU64::new(u64::MAX)),
            Arc::new(AtomicU64::new(u64::MAX)),
        ];
        for (sample, observed) in [0.25_f32, 0.5_f32].into_iter().zip(observed_frames.iter()) {
            let endpoint_source = rodio::source::UniformSourceIterator::new(
                SamplesBuffer::new(
                    NonZeroU16::new(channels).unwrap(),
                    NonZeroU32::new(sample_rate).unwrap(),
                    vec![sample; 8],
                ),
                NonZeroU16::new(channels).unwrap(),
                NonZeroU32::new(sample_rate).unwrap(),
            );
            mixer.add(SampleScheduledSource::new(
                endpoint_source,
                frame_clock.clone(),
                start_gate.clone(),
                Some(observed.clone()),
            ));
        }

        let mut output =
            SampleFrameClockSource::new(output, frame_clock.clone(), channels as usize);
        for _ in 0..(3 * channels as usize) {
            assert_eq!(output.next(), Some(0.0));
        }
        assert_eq!(frame_clock.load(Ordering::Relaxed), 3);
        assert_eq!(output.next(), Some(0.75));
        assert_eq!(output.next(), Some(0.75));
        assert_eq!(observed_frames[0].load(Ordering::Acquire), 3);
        assert_eq!(observed_frames[1].load(Ordering::Acquire), 3);
    }

    #[test]
    fn output_peak_meter_measures_the_summed_mixer_signal() {
        use std::num::{NonZeroU16, NonZeroU32};

        let (mixer, source) = rodio::mixer::mixer(
            NonZeroU16::new(1).unwrap(),
            NonZeroU32::new(48_000).unwrap(),
        );
        mixer.add(SamplesBuffer::new(
            NonZeroU16::new(1).unwrap(),
            NonZeroU32::new(48_000).unwrap(),
            vec![0.4_f32; 1_024],
        ));
        mixer.add(SamplesBuffer::new(
            NonZeroU16::new(1).unwrap(),
            NonZeroU32::new(48_000).unwrap(),
            vec![0.5_f32; 1_024],
        ));

        let peak_bits = Arc::new(AtomicU32::new(0));
        let mut metered = OutputPeakMeterSource::new(source, peak_bits.clone());
        for _ in 0..1_024 {
            assert!(metered.next().is_some());
        }

        let measured_peak = f32::from_bits(peak_bits.load(Ordering::Relaxed));
        assert!(
            (measured_peak - 0.9).abs() < 0.000_1,
            "expected the post-mix peak to be 0.9, got {measured_peak}"
        );
    }

    #[test]
    fn player_queue_preserves_six_channel_frame_alignment_from_first_sample() {
        use std::num::{NonZeroU16, NonZeroU32};

        let channels = 6_u16;
        let sample_rate = 48_000_u32;
        let (mixer, mut mixer_source) = rodio::mixer::mixer(
            NonZeroU16::new(channels).unwrap(),
            NonZeroU32::new(sample_rate).unwrap(),
        );

        // Each channel carries a distinct value. Any frame rotation is visible
        // immediately in the interleaved output.
        let expected = (0..(channels as usize * 1_200))
            .map(|sample_index| ((sample_index % channels as usize) + 1) as f32)
            .collect::<Vec<_>>();

        let (player, queue_source) = rodio::Player::new();
        player.append(SamplesBuffer::new(
            NonZeroU16::new(channels).unwrap(),
            NonZeroU32::new(sample_rate).unwrap(),
            expected.clone(),
        ));

        // The queue must report the first real source's layout before Rodio
        // constructs its UniformSourceIterator.
        assert_eq!(queue_source.channels().get(), channels as u16);

        mixer.add(FrameAlignedPlayerQueue {
            inner: queue_source,
        });
        let actual = (0..expected.len())
            .map(|_| mixer_source.next().expect("multichannel sample"))
            .collect::<Vec<_>>();

        assert_eq!(actual, expected);
        player.stop();
    }

    #[test]
    fn mixer_resampling_preserves_source_duration_across_common_rate_ratios() {
        use std::num::{NonZeroU16, NonZeroU32};

        fn assert_duration(source_rate: u32, endpoint_rate: u32) {
            let channels = 2_u16;
            let (mixer, mixer_source) = rodio::mixer::mixer(
                NonZeroU16::new(channels).unwrap(),
                NonZeroU32::new(endpoint_rate).unwrap(),
            );

            let input = SamplesBuffer::new(
                NonZeroU16::new(channels).unwrap(),
                NonZeroU32::new(source_rate).unwrap(),
                vec![0.25_f32; source_rate as usize * channels as usize],
            );
            let (player, queue_source) = rodio::Player::new();
            player.append(input);
            player.play();
            assert_eq!(queue_source.sample_rate().get(), source_rate);

            mixer.add(FrameAlignedPlayerQueue {
                inner: queue_source,
            });
            assert_eq!(mixer_source.sample_rate().get(), endpoint_rate);

            // Render two endpoint seconds. A one-second constant source should
            // occupy one endpoint second even when the rate ratio is 4:1.
            let expected_active_samples = endpoint_rate as usize * channels as usize;
            let rendered = mixer_source
                .take(expected_active_samples * 2)
                .collect::<Vec<_>>();
            let active_samples = rendered.iter().filter(|sample| sample.abs() > 0.1).count();
            let tolerance = expected_active_samples / 100;
            assert!(
                active_samples.abs_diff(expected_active_samples) <= tolerance,
                "source {} Hz -> endpoint {} Hz produced {} active samples; expected about {}",
                source_rate,
                endpoint_rate,
                active_samples,
                expected_active_samples
            );
            player.stop();
        }

        assert_duration(44_100, 48_000);
        assert_duration(48_000, 192_000);
        assert_duration(192_000, 48_000);
    }

    #[test]
    fn cue_player_queue_keeps_channel_router_destinations_frame_aligned() {
        use std::num::{NonZeroU16, NonZeroU32};

        let channels = 6_u16;
        let sample_rate = 48_000_u32;
        let (mixer, mut mixer_source) = rodio::mixer::mixer(
            NonZeroU16::new(channels).unwrap(),
            NonZeroU32::new(sample_rate).unwrap(),
        );

        let input = SamplesBuffer::new(
            NonZeroU16::new(2).unwrap(),
            NonZeroU32::new(sample_rate).unwrap(),
            vec![0.1_f32, 0.2, 0.3, 0.4, 0.5, 0.6],
        );
        let routes = Arc::new(RwLock::new(vec![
            ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: 0,
                output_channels: vec![0],
                gain_db: 0.0,
            },
            ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: 1,
                output_channels: vec![1],
                gain_db: 0.0,
            },
        ]));
        let settings = Arc::new(RwLock::new(vec![
            OutputChannelSetting::default();
            channels as usize
        ]));
        let gains = Arc::new(RwLock::new(vec![0.0; channels as usize]));
        let routed = ChannelRouter::new(input, routes, settings, gains, channels as usize);

        let expected = vec![
            0.1_f32, 0.2, 0.0, 0.0, 0.0, 0.0, 0.3, 0.4, 0.0, 0.0, 0.0, 0.0, 0.5, 0.6, 0.0, 0.0,
            0.0, 0.0,
        ];

        let (player, queue_source) = rodio::Player::new();
        player.append(routed);
        mixer.add(FrameAlignedPlayerQueue {
            inner: queue_source,
        });

        let actual = (0..expected.len())
            .map(|_| mixer_source.next().expect("routed multichannel sample"))
            .collect::<Vec<_>>();

        assert_eq!(actual, expected);
        player.stop();
    }

    #[test]
    fn mixer_output_profile_samples_complete_endpoint_frames_without_changing_samples() {
        use std::num::{NonZeroU16, NonZeroU32};

        let health = Arc::new(AudioStreamHealth::default());
        let expected = (0..4_096)
            .map(|sample| sample as f32 / 4_096.0)
            .collect::<Vec<_>>();
        let input = SamplesBuffer::new(
            NonZeroU16::new(2).unwrap(),
            NonZeroU32::new(48_000).unwrap(),
            expected.clone(),
        );
        let mut profiled = MixerOutputProfileSource::new(input, health.clone(), 2);

        let actual = profiled.by_ref().collect::<Vec<_>>();

        assert_eq!(actual, expected);
        assert_eq!(
            health.mixer_output_profile_samples.load(Ordering::Relaxed),
            2
        );
        assert_eq!(
            health.mixer_output_profiled_frames.load(Ordering::Relaxed),
            2_048
        );
    }

    #[test]
    fn channel_router_profile_samples_one_frame_per_1024_output_frames() {
        use std::num::{NonZeroU16, NonZeroU32};

        let sample_rate = 48_000_u32;
        let profile = Arc::new(AudioStreamHealth::default());
        let routes = Arc::new(RwLock::new(vec![ChannelRoute {
            active: true,
            master_enabled: true,
            output_channel: 0,
            output_channels: vec![0],
            gain_db: 0.0,
        }]));
        let settings = Arc::new(RwLock::new(vec![OutputChannelSetting::default(); 2]));
        let gains = Arc::new(RwLock::new(vec![0.0; 2]));
        let input = SamplesBuffer::new(
            NonZeroU16::new(1).unwrap(),
            NonZeroU32::new(sample_rate).unwrap(),
            vec![0.25_f32; 2_048],
        );

        let mut router = ChannelRouter::new_with_profile(
            input,
            routes,
            settings,
            gains,
            2,
            Some(profile.clone()),
        );

        assert_eq!(
            profile
                .channel_router_active_instances
                .load(Ordering::Relaxed),
            1
        );

        let rendered = (0..4_096)
            .map(|_| router.next().expect("router output sample"))
            .collect::<Vec<_>>();
        assert_eq!(rendered.len(), 4_096);
        assert_eq!(
            profile
                .channel_router_profile_samples
                .load(Ordering::Relaxed),
            2
        );
        assert_eq!(
            profile
                .channel_router_profiled_frames
                .load(Ordering::Relaxed),
            2_048
        );

        drop(router);
        assert_eq!(
            profile
                .channel_router_active_instances
                .load(Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn output_mapping_settings_keys_are_scoped_by_backend() {
        let asio_key = AudioEngine::output_device_config_key_for_backend(
            AudioBackend::Asio,
            "US-322 / US-366",
        );
        let wasapi_key = AudioEngine::output_device_config_key_for_backend(
            AudioBackend::Wasapi,
            "US-322 / US-366",
        );

        assert_ne!(asio_key, wasapi_key);
        assert!(asio_key.starts_with("ASIO::"));
        assert!(wasapi_key.starts_with("WASAPI"));
    }

    #[test]
    fn output_channel_settings_are_isolated_per_device() {
        let settings_by_device: OutputSettingsByDevice = Arc::new(RwLock::new(HashMap::new()));
        let first_device = shared_output_settings(&settings_by_device, "Interface A", 2);
        {
            let mut settings = first_device.write();
            settings[0].gain_db = -6.0;
            settings[1].active = false;
        }

        let second_device = shared_output_settings(&settings_by_device, "Interface B", 6);
        assert_eq!(second_device.read().len(), 6);
        assert_eq!(second_device.read()[0].gain_db, 0.0);
        assert!(second_device.read()[0].active);

        let restored_first = shared_output_settings(&settings_by_device, "Interface A", 2);
        assert_eq!(restored_first.read()[0].gain_db, -6.0);
        assert!(!restored_first.read()[1].active);
    }

    #[test]
    fn volume_automation_interpolates_linearly_and_clamps_edges() {
        let points = vec![[0.0, 1.0], [10.0, 0.25], [20.0, 0.75]];
        assert!((volume_automation_gain_at(-1.0, &points, true) - 1.0).abs() < 0.0001);
        assert!((volume_automation_gain_at(5.0, &points, true) - 0.625).abs() < 0.0001);
        assert!((volume_automation_gain_at(15.0, &points, true) - 0.5).abs() < 0.0001);
        assert!((volume_automation_gain_at(25.0, &points, true) - 0.75).abs() < 0.0001);
        assert!((volume_automation_gain_at(5.0, &points, false) - 1.0).abs() < 0.0001);
    }

    #[test]
    fn volume_automation_is_applied_per_stereo_frame() {
        let source = SamplesBuffer::new(
            std::num::NonZeroU16::new(2).unwrap(),
            std::num::NonZeroU32::new(2).unwrap(),
            vec![1.0_f32, 1.0, 1.0, 1.0, 1.0, 1.0],
        );
        let (_tx, rx) = unbounded();
        let (retired_tx, _retired_rx) = unbounded();
        let position_state = Arc::new(AtomicU64::new(0.0_f64.to_bits()));
        let mut automated = VolumeAutomationSource::new(
            source,
            true,
            vec![[0.0, 1.0], [1.0, 0.0]],
            rx,
            retired_tx,
            position_state.clone(),
        );
        let mut output = Vec::new();
        for position in [0.0, 0.5, 1.0] {
            atomic_store_f64(&position_state, position);
            output.push(automated.next().expect("automation sample"));
            output.push(automated.next().expect("automation sample"));
        }
        assert_eq!(output, vec![1.0, 1.0, 0.5, 0.5, 0.0, 0.0]);
    }

    #[test]
    fn volume_automation_updates_are_consumed_without_restarting_the_source() {
        let source = SamplesBuffer::new(
            std::num::NonZeroU16::new(2).unwrap(),
            std::num::NonZeroU32::new(2).unwrap(),
            vec![1.0_f32; 8],
        );
        let (tx, rx) = unbounded();
        let (retired_tx, _retired_rx) = unbounded();
        let position_state = Arc::new(AtomicU64::new(0.0_f64.to_bits()));
        let mut automated =
            VolumeAutomationSource::new(source, false, Vec::new(), rx, retired_tx, position_state);
        assert_eq!(automated.next(), Some(1.0));
        tx.send(VolumeAutomationUpdate {
            enabled: true,
            points: vec![[0.0, 0.0], [2.0, 0.0]],
        })
        .unwrap();
        // The next frame boundary consumes the live update.
        assert_eq!(automated.next(), Some(1.0));
        assert_eq!(automated.next(), Some(0.0));
        assert_eq!(automated.next(), Some(0.0));
    }

    #[test]
    fn default_stereo_pair_is_logical_and_independent_of_mapping() {
        let pair = AudioEngine::resolve_default_output_pair(None, 6);
        assert_eq!(pair, [0, 1]);
        let custom_pair = AudioEngine::resolve_default_output_pair(Some([2, 3]), 6);
        assert_eq!(custom_pair, [2, 3]);
        let order = [4, 5, 0, 1, 2, 3];
        assert_eq!(
            AudioEngine::native_output_channel_for_logical(&order, pair[0]),
            Some(4)
        );
        assert_eq!(
            AudioEngine::native_output_channel_for_logical(&order, pair[1]),
            Some(5)
        );
        assert_eq!(
            AudioEngine::resolve_default_output_pair(Some([9, 3]), 6),
            [0, 1]
        );
    }

    #[test]
    fn cue_and_test_use_identical_logical_to_native_mapping() {
        let order = [4, 5, 0, 1, 2, 3];
        let logical = [
            ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: 0,
                output_channels: vec![0],
                gain_db: 0.0,
            },
            ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: 1,
                output_channels: vec![1],
                gain_db: 0.0,
            },
        ];
        let native = AudioEngine::map_cue_routes_to_native(&logical, &order);
        assert_eq!(native[0].output_channels, vec![4]);
        assert_eq!(native[1].output_channels, vec![5]);
        assert_eq!(
            native[0].output_channel,
            AudioEngine::native_output_channel_for_logical(&order, 0).unwrap()
        );
        assert_eq!(
            native[1].output_channel,
            AudioEngine::native_output_channel_for_logical(&order, 1).unwrap()
        );
        assert_eq!(logical[0].output_channels, vec![0]);
        assert_eq!(logical[1].output_channels, vec![1]);
        assert_eq!(
            AudioEngine::map_cue_gains_to_native(&[-6.0, -3.0, 0.0, 0.0, 0.0, 0.0], &order),
            vec![0.0, 0.0, 0.0, 0.0, -6.0, -3.0]
        );
    }

    #[test]
    fn changing_mapping_does_not_rewrite_logical_cue_or_default_pair() {
        let saved_stereo_pair = [2, 3]; // TaSQ Outputs 3/4
        let saved_logical_routes = vec![
            ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: saved_stereo_pair[0],
                output_channels: vec![saved_stereo_pair[0]],
                gain_db: 0.0,
            },
            ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: saved_stereo_pair[1],
                output_channels: vec![saved_stereo_pair[1]],
                gain_db: 0.0,
            },
        ];
        let first_map = [0, 1, 2, 3, 4, 5];
        let second_map = [4, 5, 0, 1, 2, 3];
        let first = AudioEngine::map_cue_routes_to_native(&saved_logical_routes, &first_map);
        let second = AudioEngine::map_cue_routes_to_native(&saved_logical_routes, &second_map);
        assert_eq!(first[0].output_channels, vec![2]);
        assert_eq!(first[1].output_channels, vec![3]);
        assert_eq!(second[0].output_channels, vec![0]);
        assert_eq!(second[1].output_channels, vec![1]);
        assert_eq!(saved_stereo_pair, [2, 3]);
        assert_eq!(saved_logical_routes[0].output_channels, vec![2]);
        assert_eq!(saved_logical_routes[1].output_channels, vec![3]);
    }

    #[test]
    fn backend_buffer_choices_match_each_backend_control_model() {
        assert_eq!(
            buffer_size_options_for_backend(AudioBackend::Wasapi, &[480]),
            vec![256, 512, 1_024, 2_048, 4_096, 8_192]
        );
        assert_eq!(
            buffer_size_options_for_backend(AudioBackend::DirectSound, &[480]),
            vec![1_024, 2_048, 4_096, 8_192, 16_384]
        );
        assert_eq!(
            buffer_size_options_for_backend(AudioBackend::Asio, &[128, 256, 512]),
            vec![128, 256, 512]
        );
        assert!(buffer_size_options_for_backend(AudioBackend::Asio, &[]).is_empty());
    }

    #[test]
    fn other_asio_devices_keep_native_channel_order_by_default() {
        let order =
            AudioEngine::default_output_channel_order_for(AudioBackend::Asio, "RME Fireface", 6);
        assert_eq!(order, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn wasapi_devices_keep_native_channel_order_by_default() {
        let order = AudioEngine::default_output_channel_order_for(
            AudioBackend::Wasapi,
            "US-322 / US-366",
            6,
        );
        assert_eq!(order, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn raw_test_tone_keeps_selected_channel_stable_past_32768_samples() {
        use rodio::source::UniformSourceIterator;

        let current_generation = Arc::new(AtomicU64::new(1));
        let source = RawOutputTestTone {
            output_channels: 6,
            selected_channel: 4,
            sample_rate: 48_000,
            sample_index: 0,
            duration_frames: 8_000,
            fade_frames: 384,
            generation: 1,
            current_generation,
            stop_frame: None,
        };
        let mut uniform = UniformSourceIterator::new(
            source,
            std::num::NonZeroU16::new(6).unwrap(),
            std::num::NonZeroU32::new(48_000).unwrap(),
        );

        let mut selected_channel_had_signal = false;
        // 7,000 frames = 42,000 samples, crossing Rodio's former 32,768-sample
        // internal span boundary. The tone must remain on channel index 4.
        for _ in 0..7_000 {
            for channel in 0..6 {
                let sample = uniform.next().expect("continuous test tone");
                if channel == 4 {
                    selected_channel_had_signal |= sample.abs() > 0.01;
                } else {
                    assert_eq!(sample, 0.0, "unexpected signal on channel {channel}");
                }
            }
        }
        assert!(selected_channel_had_signal);
    }

    #[test]
    fn raw_test_tone_cancellation_fades_and_stays_on_selected_channel() {
        let current_generation = Arc::new(AtomicU64::new(1));
        let mut source = RawOutputTestTone {
            output_channels: 6,
            selected_channel: 2,
            sample_rate: 48_000,
            sample_index: 0,
            duration_frames: 8_000,
            fade_frames: 4,
            generation: 1,
            current_generation: current_generation.clone(),
            stop_frame: None,
        };

        // Advance to a non-zero part of the waveform, then cancel it.
        for _ in 0..12 {
            let _ = source.next().expect("test tone sample");
        }
        current_generation.store(2, Ordering::Relaxed);

        let mut saw_selected_signal = false;
        for frame in 0..8 {
            for channel in 0..6 {
                let sample = source.next();
                if frame >= 4 {
                    assert!(
                        sample.is_none(),
                        "cancelled test tone should finish after fade"
                    );
                    continue;
                }
                let sample = sample.expect("fade samples");
                if channel == 2 {
                    saw_selected_signal |= sample.abs() > 0.0001;
                } else {
                    assert_eq!(sample, 0.0, "unexpected signal on channel {channel}");
                }
            }
        }
        assert!(saw_selected_signal);
    }

    #[test]
    fn channel_router_refreshes_live_routing_into_its_render_snapshot() {
        let routes = Arc::new(RwLock::new(vec![ChannelRoute {
            active: true,
            master_enabled: true,
            output_channel: 0,
            output_channels: vec![0],
            gain_db: 0.0,
        }]));
        let output_settings = Arc::new(RwLock::new(vec![
            OutputChannelSetting::default(),
            OutputChannelSetting::default(),
        ]));
        let output_gains = Arc::new(RwLock::new(vec![0.0, 0.0]));
        let source = SamplesBuffer::new(
            std::num::NonZeroU16::new(1).unwrap(),
            std::num::NonZeroU32::new(48_000).unwrap(),
            vec![0.5_f32],
        );
        let mut router =
            ChannelRouter::new(source, routes.clone(), output_settings, output_gains, 2);

        routes.write()[0] = ChannelRoute {
            active: true,
            master_enabled: true,
            output_channel: 1,
            output_channels: vec![1],
            gain_db: 0.0,
        };
        router.refresh_live_settings();

        let frame = [
            router.next().expect("left output sample"),
            router.next().expect("right output sample"),
        ];
        assert_eq!(frame, [0.0, 0.5]);
    }

    #[test]
    fn channel_router_keeps_last_snapshot_when_control_thread_holds_write_lock() {
        let routes = Arc::new(RwLock::new(vec![ChannelRoute {
            active: true,
            master_enabled: true,
            output_channel: 0,
            output_channels: vec![0],
            gain_db: 0.0,
        }]));
        let output_settings = Arc::new(RwLock::new(vec![
            OutputChannelSetting::default(),
            OutputChannelSetting::default(),
        ]));
        let output_gains = Arc::new(RwLock::new(vec![0.0, 0.0]));
        let source = SamplesBuffer::new(
            std::num::NonZeroU16::new(1).unwrap(),
            std::num::NonZeroU32::new(48_000).unwrap(),
            vec![0.5_f32],
        );
        let mut router =
            ChannelRouter::new(source, routes.clone(), output_settings, output_gains, 2);

        let _write_guard = routes.write();
        // This must return immediately instead of waiting for the writer.
        router.refresh_live_settings();
        let frame = [
            router.next().expect("left output sample"),
            router.next().expect("right output sample"),
        ];
        assert_eq!(frame, [0.5, 0.0]);
    }

    #[test]
    fn channel_router_keeps_six_channel_frame_alignment_across_rodio_spans() {
        use rodio::source::UniformSourceIterator;

        let routes = Arc::new(RwLock::new(vec![
            ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: 4,
                output_channels: vec![4],
                gain_db: 0.0,
            },
            ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: 5,
                output_channels: vec![5],
                gain_db: 0.0,
            },
        ]));
        let output_settings = Arc::new(RwLock::new(vec![OutputChannelSetting::default(); 6]));
        let input_samples = (0..8_000)
            .flat_map(|_| [0.25_f32, 0.75_f32])
            .collect::<Vec<_>>();
        let source = SamplesBuffer::new(
            std::num::NonZeroU16::new(2).unwrap(),
            std::num::NonZeroU32::new(48_000).unwrap(),
            input_samples,
        );
        let output_gains = Arc::new(RwLock::new(vec![0.0; 6]));
        let router = ChannelRouter::new(source, routes, output_settings, output_gains, 6);
        let mut uniform = UniformSourceIterator::new(
            router,
            std::num::NonZeroU16::new(6).unwrap(),
            std::num::NonZeroU32::new(48_000).unwrap(),
        );

        // Cross the 32,768-sample boundary while checking that L/R remain on 5/6.
        for _ in 0..6_000 {
            let frame = (0..6)
                .map(|_| uniform.next().expect("routed sample"))
                .collect::<Vec<_>>();
            assert_eq!(&frame[..4], &[0.0, 0.0, 0.0, 0.0]);
            assert_eq!(frame[4], 0.25);
            assert_eq!(frame[5], 0.75);
        }
    }

    #[test]
    fn detects_physical_stereo_pair_from_driver_channel_names() {
        let channels = vec![
            OutputChannelInfo {
                index: 0,
                name: "Output 1".to_string(),
                group: "0".to_string(),
                active: false,
            },
            OutputChannelInfo {
                index: 1,
                name: "Output 2".to_string(),
                group: "0".to_string(),
                active: false,
            },
            OutputChannelInfo {
                index: 2,
                name: "Output 3".to_string(),
                group: "1".to_string(),
                active: false,
            },
            OutputChannelInfo {
                index: 3,
                name: "Output 4".to_string(),
                group: "1".to_string(),
                active: false,
            },
            OutputChannelInfo {
                index: 4,
                name: "LINE OUT 1/L".to_string(),
                group: "2".to_string(),
                active: false,
            },
            OutputChannelInfo {
                index: 5,
                name: "LINE OUT 2/R".to_string(),
                group: "2".to_string(),
                active: false,
            },
        ];

        assert_eq!(
            AudioEngine::detect_default_stereo_pair(&channels),
            Some([4, 5])
        );
    }

    #[test]
    fn does_not_guess_a_physical_pair_from_generic_asio_names() {
        let channels = vec![
            OutputChannelInfo {
                index: 0,
                name: "Output 1".to_string(),
                group: "0".to_string(),
                active: false,
            },
            OutputChannelInfo {
                index: 1,
                name: "Output 2".to_string(),
                group: "0".to_string(),
                active: false,
            },
        ];

        assert_eq!(AudioEngine::detect_default_stereo_pair(&channels), None);
    }

    #[test]
    fn track_routes_map_source_channels_to_selected_hardware_outputs() {
        let routes = Arc::new(RwLock::new(vec![
            ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: 2,
                output_channels: vec![2],
                gain_db: 0.0,
            },
            ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: 3,
                output_channels: vec![3],
                gain_db: 0.0,
            },
        ]));
        let output_settings = Arc::new(RwLock::new(vec![OutputChannelSetting::default(); 4]));
        let source = SamplesBuffer::new(
            std::num::NonZeroU16::new(2).unwrap(),
            std::num::NonZeroU32::new(48_000).unwrap(),
            vec![0.25_f32, 0.75_f32],
        );
        let output_gains = Arc::new(RwLock::new(vec![0.0; 4]));
        let router = ChannelRouter::new(source, routes, output_settings, output_gains, 4);
        let output: Vec<f32> = router.take(4).collect();

        assert_eq!(output, vec![0.0, 0.0, 0.25, 0.75]);
    }

    #[test]
    fn channel_router_applies_event_output_gain_per_hardware_channel() {
        let routes = Arc::new(RwLock::new(vec![ChannelRoute {
            active: true,
            master_enabled: true,
            output_channel: 0,
            output_channels: vec![0, 1],
            gain_db: 0.0,
        }]));
        let output_settings = Arc::new(RwLock::new(vec![OutputChannelSetting::default(); 2]));
        let output_gains = Arc::new(RwLock::new(vec![-6.0, 0.0]));
        let source = SamplesBuffer::new(
            std::num::NonZeroU16::new(1).unwrap(),
            std::num::NonZeroU32::new(48_000).unwrap(),
            vec![1.0_f32],
        );
        let router = ChannelRouter::new(source, routes, output_settings, output_gains, 2);
        let frame: Vec<f32> = router.take(2).collect();
        assert!((frame[0] - db_to_linear(-6.0)).abs() < 0.0001);
        assert!((frame[1] - 1.0).abs() < 0.0001);
    }

    #[test]
    fn channel_router_can_fan_out_mono_to_multiple_outputs() {
        let routes = Arc::new(RwLock::new(vec![ChannelRoute {
            active: true,
            master_enabled: true,
            output_channel: 0,
            output_channels: vec![0, 1, 2, 3, 4, 5],
            gain_db: 0.0,
        }]));
        let output_settings = Arc::new(RwLock::new(vec![OutputChannelSetting::default(); 6]));
        let source = SamplesBuffer::new(
            std::num::NonZeroU16::new(1).unwrap(),
            std::num::NonZeroU32::new(48_000).unwrap(),
            vec![0.5_f32],
        );
        let output_gains = Arc::new(RwLock::new(vec![0.0; 6]));
        let router = ChannelRouter::new(source, routes, output_settings, output_gains, 6);
        let frame: Vec<f32> = router.take(6).collect();
        assert_eq!(frame, vec![0.5; 6]);
    }

    #[test]
    fn channel_router_can_downmix_all_source_channels_to_one_output() {
        let routes = Arc::new(RwLock::new(vec![
            ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: 0,
                output_channels: vec![0],
                gain_db: 0.0,
            },
            ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: 0,
                output_channels: vec![0],
                gain_db: 0.0,
            },
        ]));
        let output_settings = Arc::new(RwLock::new(vec![OutputChannelSetting::default(); 2]));
        let source = SamplesBuffer::new(
            std::num::NonZeroU16::new(2).unwrap(),
            std::num::NonZeroU32::new(48_000).unwrap(),
            vec![0.25_f32, 0.75_f32],
        );
        let output_gains = Arc::new(RwLock::new(vec![0.0; 2]));
        let router = ChannelRouter::new(source, routes, output_settings, output_gains, 2);
        let output: Vec<f32> = router.take(2).collect();
        assert_eq!(output, vec![1.0, 0.0]);
    }

    #[test]
    fn master_disabled_source_channel_is_not_sent() {
        let routes = Arc::new(RwLock::new(vec![ChannelRoute {
            active: true,
            master_enabled: false,
            output_channel: 0,
            output_channels: vec![0],
            gain_db: 0.0,
        }]));
        let output_settings = Arc::new(RwLock::new(vec![OutputChannelSetting::default()]));
        let source = SamplesBuffer::new(
            std::num::NonZeroU16::new(1).unwrap(),
            std::num::NonZeroU32::new(48_000).unwrap(),
            vec![0.75_f32],
        );
        let output_gains = Arc::new(RwLock::new(vec![0.0]));
        let router = ChannelRouter::new(source, routes, output_settings, output_gains, 1);
        let output: Vec<f32> = router.take(1).collect();

        assert_eq!(output, vec![0.0]);
    }

    #[test]
    fn inactive_track_source_channel_is_not_sent() {
        let routes = Arc::new(RwLock::new(vec![
            ChannelRoute {
                active: false,
                master_enabled: true,
                output_channel: 0,
                output_channels: Vec::new(),
                gain_db: 0.0,
            },
            ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: 1,
                output_channels: vec![1],
                gain_db: 0.0,
            },
        ]));
        let output_settings = Arc::new(RwLock::new(vec![OutputChannelSetting::default(); 2]));
        let source = SamplesBuffer::new(
            std::num::NonZeroU16::new(2).unwrap(),
            std::num::NonZeroU32::new(48_000).unwrap(),
            vec![0.5_f32, 0.75_f32],
        );
        let output_gains = Arc::new(RwLock::new(vec![0.0; 2]));
        let router = ChannelRouter::new(source, routes, output_settings, output_gains, 2);
        let output: Vec<f32> = router.take(2).collect();

        assert_eq!(output, vec![0.0, 0.75]);
    }
}

#[cfg(test)]
mod time_pitch_tests {
    use super::*;

    #[test]
    fn speed_and_pitch_automation_interpolate_and_clamp() {
        let speed = vec![[0.0, 1.0], [10.0, 2.0]];
        assert!((automation_value_at(5.0, &speed, true, 1.0) - 1.5).abs() < 0.0001);
        assert!((automation_value_at(20.0, &speed, true, 1.0) - 2.0).abs() < 0.0001);
        assert!((automation_value_at(5.0, &speed, false, 1.25) - 1.25).abs() < 0.0001);

        let pitch = vec![[0.0, -1200.0], [4.0, 1200.0]];
        assert!(automation_value_at(2.0, &pitch, true, 0.0).abs() < 0.0001);
    }

    #[test]
    fn neutral_time_pitch_is_an_exact_passthrough() {
        let samples = vec![0.1_f32, -0.2, 0.3, -0.4, 0.5, -0.6];
        let source = rodio::buffer::SamplesBuffer::new(
            std::num::NonZeroU16::new(1).unwrap(),
            std::num::NonZeroU32::new(48_000).unwrap(),
            samples.clone(),
        );
        let settings = Arc::new(RwLock::new(TimePitchSettings::default()));
        let position = Arc::new(AtomicU64::new(0.0_f64.to_bits()));
        let output: Vec<f32> =
            SpeedPitchSource::new(source, 0.0, settings, position.clone()).collect();
        assert_eq!(output, samples);
        let expected_position = samples.len() as f64 / 48_000.0;
        assert!((atomic_load_f64(&position) - expected_position).abs() < 1e-9);
    }

    struct PartialInterleavedSource {
        samples: std::vec::IntoIter<f32>,
    }

    impl Iterator for PartialInterleavedSource {
        type Item = f32;

        fn next(&mut self) -> Option<Self::Item> {
            self.samples.next()
        }
    }

    impl Source for PartialInterleavedSource {
        fn current_span_len(&self) -> Option<usize> {
            Some(self.samples.len())
        }

        fn channels(&self) -> std::num::NonZeroU16 {
            std::num::NonZeroU16::new(2).unwrap()
        }

        fn sample_rate(&self) -> std::num::NonZeroU32 {
            std::num::NonZeroU32::new(48_000).unwrap()
        }

        fn total_duration(&self) -> Option<Duration> {
            None
        }
    }

    #[test]
    fn phase_vocoder_input_refill_pads_a_partial_interleaved_frame() {
        let source = PartialInterleavedSource {
            samples: vec![0.1_f32, -0.1, 0.2].into_iter(),
        };
        let settings = Arc::new(RwLock::new(TimePitchSettings::default()));
        let position = Arc::new(AtomicU64::new(0.0_f64.to_bits()));
        let mut source = SpeedPitchSource::new(source, 0.0, settings, position);

        source.fill_input_until(2);

        assert!(source.input_eof);
        assert_eq!(
            source.input_samples.iter().copied().collect::<Vec<_>>(),
            vec![0.1_f32, -0.1, 0.2, 0.0]
        );
        assert_eq!(source.available_end_frame(), 2);
    }

    #[test]
    fn phase_vocoder_speed_processing_produces_finite_audio() {
        let samples: Vec<f32> = (0..4096)
            .map(|index| (std::f32::consts::TAU * 440.0 * index as f32 / 48_000.0).sin())
            .collect();
        let source = rodio::buffer::SamplesBuffer::new(
            std::num::NonZeroU16::new(1).unwrap(),
            std::num::NonZeroU32::new(48_000).unwrap(),
            samples,
        );
        let settings = Arc::new(RwLock::new(TimePitchSettings {
            speed: 2.0,
            ..TimePitchSettings::default()
        }));
        let position = Arc::new(AtomicU64::new(0.0_f64.to_bits()));
        let output: Vec<f32> = SpeedPitchSource::new(source, 0.0, settings, position).collect();
        assert!(!output.is_empty());
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    struct SeekableSamplesSource {
        samples: Vec<f32>,
        position: usize,
        channels: std::num::NonZeroU16,
        sample_rate: std::num::NonZeroU32,
    }

    impl Iterator for SeekableSamplesSource {
        type Item = f32;

        fn next(&mut self) -> Option<Self::Item> {
            let sample = self.samples.get(self.position).copied();
            if sample.is_some() {
                self.position += 1;
            }
            sample
        }
    }

    impl Source for SeekableSamplesSource {
        fn current_span_len(&self) -> Option<usize> {
            Some(self.samples.len().saturating_sub(self.position))
        }

        fn channels(&self) -> std::num::NonZeroU16 {
            self.channels
        }

        fn sample_rate(&self) -> std::num::NonZeroU32 {
            self.sample_rate
        }

        fn total_duration(&self) -> Option<Duration> {
            Some(Duration::from_secs_f64(
                self.samples.len() as f64
                    / self.channels.get() as f64
                    / self.sample_rate.get() as f64,
            ))
        }

        fn try_seek(&mut self, position: Duration) -> Result<(), rodio::source::SeekError> {
            let frame = (position.as_secs_f64() * self.sample_rate.get() as f64).round() as usize;
            self.position = frame
                .saturating_mul(self.channels.get() as usize)
                .min(self.samples.len());
            Ok(())
        }
    }

    struct NonSeekableSamplesSource {
        samples: Vec<f32>,
        position: usize,
        channels: std::num::NonZeroU16,
        sample_rate: std::num::NonZeroU32,
    }

    impl Iterator for NonSeekableSamplesSource {
        type Item = f32;

        fn next(&mut self) -> Option<Self::Item> {
            let sample = self.samples.get(self.position).copied();
            if sample.is_some() {
                self.position += 1;
            }
            sample
        }
    }

    impl Source for NonSeekableSamplesSource {
        fn current_span_len(&self) -> Option<usize> {
            Some(self.samples.len().saturating_sub(self.position))
        }

        fn channels(&self) -> std::num::NonZeroU16 {
            self.channels
        }

        fn sample_rate(&self) -> std::num::NonZeroU32 {
            self.sample_rate
        }

        fn total_duration(&self) -> Option<Duration> {
            Some(Duration::from_secs_f64(
                self.samples.len() as f64
                    / self.channels.get() as f64
                    / self.sample_rate.get() as f64,
            ))
        }
    }

    fn test_seekable_source_with_channels(
        samples: Vec<f32>,
        sample_rate: u32,
        channels: u16,
    ) -> SeekableSamplesSource {
        SeekableSamplesSource {
            samples,
            position: 0,
            channels: std::num::NonZeroU16::new(channels.max(1)).unwrap(),
            sample_rate: std::num::NonZeroU32::new(sample_rate).unwrap(),
        }
    }

    fn test_seekable_source(samples: Vec<f32>, sample_rate: u32) -> SeekableSamplesSource {
        test_seekable_source_with_channels(samples, sample_rate, 1)
    }

    #[test]
    fn finite_loop_repeats_region_then_continues_after_loop_end_without_stream_restart() {
        let source = test_seekable_source(vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0], 1);
        let loops_remaining = Arc::new(AtomicI32::new(3));
        let loop_iterations = Arc::new(AtomicU64::new(0));
        let looping = LoopingSource::new(
            source,
            0.0,
            1.0,
            3.0,
            true,
            loops_remaining.clone(),
            loop_iterations.clone(),
            "test-finite-loop".to_string(),
        );
        let timeline = looping.timeline.clone();
        let settings = Arc::new(RwLock::new(TimePitchSettings::default()));
        let position = Arc::new(AtomicU64::new(0.0_f64.to_bits()));
        let producer_remaining = Arc::new(AtomicI32::new(0));
        let output = SpeedPitchSource::new_with_loop(
            looping,
            0.0,
            settings,
            position.clone(),
            timeline,
            producer_remaining.clone(),
        )
        .collect::<Vec<_>>();

        assert_eq!(
            output,
            vec![0.0, 1.0, 2.0, 1.0, 2.0, 1.0, 2.0, 3.0, 4.0, 5.0]
        );
        assert_eq!(producer_remaining.load(Ordering::Acquire), 1);
        assert_eq!(loops_remaining.load(Ordering::Acquire), 1);
        assert_eq!(loop_iterations.load(Ordering::Acquire), 2);
        assert!((atomic_load_f64(&position) - 6.0).abs() < f64::EPSILON);
    }

    #[test]
    fn infinite_loop_keeps_one_source_alive_and_wraps_at_exact_frame_boundary() {
        let source = test_seekable_source(vec![0.0, 1.0, 2.0, 3.0, 4.0], 1);
        let loops_remaining = Arc::new(AtomicI32::new(-1));
        let loop_iterations = Arc::new(AtomicU64::new(0));
        let looping = LoopingSource::new(
            source,
            0.0,
            1.0,
            3.0,
            true,
            loops_remaining.clone(),
            loop_iterations.clone(),
            "test-infinite-loop".to_string(),
        );
        let timeline = looping.timeline.clone();
        let settings = Arc::new(RwLock::new(TimePitchSettings::default()));
        let position = Arc::new(AtomicU64::new(0.0_f64.to_bits()));
        let producer_remaining = Arc::new(AtomicI32::new(0));
        let output = SpeedPitchSource::new_with_loop(
            looping,
            0.0,
            settings,
            position.clone(),
            timeline,
            producer_remaining.clone(),
        )
        .take(8)
        .collect::<Vec<_>>();

        assert_eq!(output, vec![0.0, 1.0, 2.0, 1.0, 2.0, 1.0, 2.0, 1.0]);
        assert_eq!(producer_remaining.load(Ordering::Acquire), -1);
        assert_eq!(loops_remaining.load(Ordering::Acquire), -1);
        assert_eq!(loop_iterations.load(Ordering::Acquire), 3);
        assert!((atomic_load_f64(&position) - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn loop_source_honors_nonzero_initial_offset_before_repeating_region() {
        let source = test_seekable_source(vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0], 1);
        let loops_remaining = Arc::new(AtomicI32::new(2));
        let loop_iterations = Arc::new(AtomicU64::new(0));
        let looping = LoopingSource::new(
            source,
            2.0,
            1.0,
            4.0,
            true,
            loops_remaining.clone(),
            loop_iterations.clone(),
            "test-offset-loop".to_string(),
        );
        let timeline = looping.timeline.clone();
        let settings = Arc::new(RwLock::new(TimePitchSettings::default()));
        let position = Arc::new(AtomicU64::new(0.0_f64.to_bits()));
        let output = SpeedPitchSource::new_with_loop(
            looping,
            2.0,
            settings,
            position.clone(),
            timeline,
            Arc::new(AtomicI32::new(0)),
        )
        .collect::<Vec<_>>();

        assert_eq!(output, vec![2.0, 3.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(loops_remaining.load(Ordering::Acquire), 1);
        assert_eq!(loop_iterations.load(Ordering::Acquire), 1);
        assert!((atomic_load_f64(&position) - 6.0).abs() < f64::EPSILON);
    }

    #[test]
    fn stereo_loop_boundaries_never_split_interleaved_frames() {
        let source = test_seekable_source_with_channels(
            vec![0.0, 10.0, 1.0, 11.0, 2.0, 12.0, 3.0, 13.0, 4.0, 14.0],
            1,
            2,
        );
        let loops_remaining = Arc::new(AtomicI32::new(2));
        let loop_iterations = Arc::new(AtomicU64::new(0));
        let looping = LoopingSource::new(
            source,
            0.0,
            1.0,
            3.0,
            true,
            loops_remaining,
            loop_iterations,
            "test-stereo-loop".to_string(),
        );
        let output = looping.collect::<Vec<_>>();
        assert_eq!(
            output,
            vec![0.0, 10.0, 1.0, 11.0, 2.0, 12.0, 1.0, 11.0, 2.0, 12.0, 3.0, 13.0, 4.0, 14.0,]
        );
    }

    #[test]
    fn loop_seek_unsupported_disables_repeats_but_keeps_source_playing() {
        let source = NonSeekableSamplesSource {
            samples: vec![0.0, 1.0, 2.0, 3.0, 4.0],
            position: 0,
            channels: std::num::NonZeroU16::new(1).unwrap(),
            sample_rate: std::num::NonZeroU32::new(1).unwrap(),
        };
        let loops_remaining = Arc::new(AtomicI32::new(2));
        let loop_iterations = Arc::new(AtomicU64::new(0));
        let looping = LoopingSource::new(
            source,
            0.0,
            1.0,
            3.0,
            true,
            loops_remaining.clone(),
            loop_iterations.clone(),
            "test-nonseekable-loop".to_string(),
        );
        let timeline = looping.timeline.clone();
        let settings = Arc::new(RwLock::new(TimePitchSettings::default()));
        let position = Arc::new(AtomicU64::new(0.0_f64.to_bits()));
        let producer_remaining = Arc::new(AtomicI32::new(0));
        let output = SpeedPitchSource::new_with_loop(
            looping,
            0.0,
            settings,
            position.clone(),
            timeline,
            producer_remaining.clone(),
        )
        .collect::<Vec<_>>();

        assert_eq!(output, vec![0.0, 1.0, 2.0, 3.0, 4.0]);
        assert_eq!(producer_remaining.load(Ordering::Acquire), 1);
        assert_eq!(loops_remaining.load(Ordering::Acquire), 1);
        assert_eq!(loop_iterations.load(Ordering::Acquire), 0);
        assert!((atomic_load_f64(&position) - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn loop_timeline_maps_each_pass_and_the_final_tail() {
        let timeline = LoopTimeline {
            initial_start_frame: 0,
            loop_start_frame: 2,
            loop_end_frame: 5,
            initial_loops_remaining: 3,
            failure: Arc::new(LoopSeekFailure {
                input_frame: AtomicU64::new(u64::MAX),
                media_frame: AtomicU64::new(0),
            }),
        };
        let mapped = (0..12)
            .map(|frame| timeline.media_frame_at_input_frame(frame as f64))
            .collect::<Vec<_>>();
        assert_eq!(
            mapped,
            vec![0.0, 1.0, 2.0, 3.0, 4.0, 2.0, 3.0, 4.0, 2.0, 3.0, 4.0, 5.0]
        );
        let remaining = (0..12)
            .map(|frame| timeline.loops_remaining_at_input_frame(frame as f64))
            .collect::<Vec<_>>();
        assert_eq!(remaining, vec![3, 3, 3, 3, 3, 2, 2, 2, 1, 1, 1, 1]);
    }

    #[test]
    fn loop_fade_out_is_suppressed_until_the_final_pass() {
        assert!(!should_apply_loop_fade_out(true, 3, 12.0, 12.0));
        assert!(!should_apply_loop_fade_out(true, -1, 12.0, 12.0));
        assert!(should_apply_loop_fade_out(true, 1, 12.0, 12.0));
        assert!(should_apply_loop_fade_out(true, -1, 8.0, 12.0));
        assert!(should_apply_loop_fade_out(false, 0, 12.0, 12.0));
    }

    #[test]
    fn prefetch_frame_positions_preserve_a_loop_wrap_inside_one_chunk() {
        let chunk = PcmPrefetchChunk {
            samples: vec![0.0; 3],
            frame_end_positions: vec![1.0_f64.to_bits(), 0.1_f64.to_bits(), 0.2_f64.to_bits()],
            frame_loop_remaining: vec![2, 1, 1],
            start_position_bits: 0.9_f64.to_bits(),
            end_position_bits: 0.2_f64.to_bits(),
            start_loop_remaining: 2,
            eof: false,
        };
        assert!((prefetch_chunk_frame_start_position(&chunk, 0) - 0.9).abs() < f64::EPSILON);
        assert!((prefetch_chunk_frame_start_position(&chunk, 1) - 1.0).abs() < f64::EPSILON);
        assert!((prefetch_chunk_frame_start_position(&chunk, 2) - 0.1).abs() < f64::EPSILON);
        assert_eq!(prefetch_chunk_frame_start_loop_remaining(&chunk, 0), 2);
        assert_eq!(prefetch_chunk_frame_start_loop_remaining(&chunk, 2), 1);
    }

    #[test]
    fn sanitizer_keeps_time_pitch_values_in_supported_ranges() {
        let mut settings = TimePitchSettings {
            speed: 9.0,
            pitch_cents: -5000.0,
            speed_automation: vec![[2.0, 8.0], [1.0, 0.01]],
            pitch_automation: vec![[0.0, 5000.0]],
            ..TimePitchSettings::default()
        };
        sanitize_time_pitch_settings(&mut settings);
        assert_eq!(settings.speed, 4.0);
        assert_eq!(settings.pitch_cents, -2400.0);
        assert_eq!(settings.speed_automation[0], [1.0, 0.1]);
        assert_eq!(settings.speed_automation[1], [2.0, 4.0]);
        assert_eq!(settings.pitch_automation[0], [0.0, 2400.0]);
    }
}

// Isolated test-only prototype: it is deliberately not wired into production playback.
#[cfg(test)]
#[path = "sparse_mixer.rs"]
mod sparse_mixer_prototype;

// Stage 4.1 runtime contract model. Test-only: it is not connected to playback.
#[cfg(test)]
#[path = "sparse_runtime_contract.rs"]
mod sparse_runtime_contract;

// Stage 4.2a command pump. Test-only: no PCM rendering or production playback.
#[cfg(test)]
#[path = "sparse_runtime_pump.rs"]
mod sparse_runtime_pump;

// Stage 4.2b1 bounded Reaper handoff. Test-only: no production resource ownership.
#[cfg(test)]
#[path = "sparse_runtime_reaper.rs"]
mod sparse_runtime_reaper;

// Stage 4.2b2 move-only Cue source ownership. Test-only: no production playback integration.
#[cfg(test)]
#[path = "sparse_runtime_ownership.rs"]
mod sparse_runtime_ownership;

// Stage 4.2b3 sparse endpoint PCM renderer. Test-only: no production integration.
#[cfg(test)]
#[path = "sparse_runtime_renderer.rs"]
mod sparse_runtime_renderer;

// Stage 4.3a atomic Cue control bridge. Test-only: no production integration.
#[cfg(test)]
#[path = "sparse_runtime_control.rs"]
mod sparse_runtime_control;
