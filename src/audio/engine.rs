// src/audio/engine.rs — аудиодвижок с поддержкой Fade In/Out, Start/End, Loop и смены I/O карт

use crate::audio::device_registry::{
    AudioEndpoint, DeviceAvailability, DeviceRegistry, DeviceScanResult,
};
use crate::audio::OutputChannelInfo;
use crate::config::AudioBackend;
use crate::cue::{ChannelRoute, CueState, FadeCurve, OutputChannelSetting, TimePitchSettings};
use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, Host};
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

type SharedOutputSettings = Arc<RwLock<Vec<OutputChannelSetting>>>;
type OutputSettingsByDevice = Arc<RwLock<HashMap<String, SharedOutputSettings>>>;

#[derive(Debug, Clone)]
pub enum AudioCommand {
    Play {
        id: Uuid,
        /// Monotonic per-cue generation used to discard stale position updates after Stop/restart.
        generation: u64,
        file: PathBuf,
        /// None uses the global output device; Some selects a per-cue device.
        output_device: Option<String>,
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
    Shutdown,
}

#[derive(Debug, Clone)]
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

fn preferred_output_config(device: &Device) -> Option<cpal::SupportedStreamConfig> {
    // Select the configuration with the greatest number of channels first;
    // prefer F32 only when two configurations expose the same channel count.
    let config = device
        .supported_output_configs()
        .ok()?
        .max_by_key(|config| {
            let min_rate = config.min_sample_rate();
            let max_rate = config.max_sample_rate();
            let rate_priority = if min_rate <= 48_000 && max_rate >= 48_000 {
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
    let min_rate = config.min_sample_rate();
    let max_rate = config.max_sample_rate();
    let sample_rate = if min_rate <= 48_000 && max_rate >= 48_000 {
        48_000
    } else if min_rate <= 44_100 && max_rate >= 44_100 {
        44_100
    } else {
        max_rate
    };
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
    output_device_channels: HashMap<String, usize>,
    output_device_channel_layouts: HashMap<String, Vec<usize>>,
    /// Per-device output mute/trim settings shared with the audio worker.
    output_settings_by_device: OutputSettingsByDevice,
    /// Optional channel names reported by the native ASIO driver, keyed by CPAL device name.
    output_driver_channel_info: HashMap<String, Vec<OutputChannelInfo>>,
    /// User-defined aliases override driver-provided names but never change routing indices.
    output_channel_names: HashMap<String, Vec<String>>,
    default_output_pairs: HashMap<String, [usize; 2]>,
    /// Logical Theatre Show Cue output order -> native CPAL/ASIO stream channel index.
    /// Stored per device so non-standard driver orders do not affect other interfaces.
    output_channel_orders: HashMap<String, Vec<usize>>,
    detected_default_output_pairs: HashMap<String, [usize; 2]>,
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
        let device_registry = DeviceRegistry::from_scan(Self::scan_output_endpoints());
        Self::new_with_backend_and_endpoint_using_registry(
            backend,
            selected_device_id,
            selected_device,
            device_registry,
        )
    }

    /// Switch the active backend using the already-populated cross-backend
    /// registry. This avoids probing ASIO again while the previous ASIO stream
    /// is still open and preserves endpoints when a backend scan is transiently
    /// unavailable during the switch.
    pub fn new_with_backend_and_endpoint_using_registry(
        backend: AudioBackend,
        selected_device_id: Option<&str>,
        selected_device: Option<&str>,
        mut device_registry: DeviceRegistry,
    ) -> Result<Self> {
        if backend == AudioBackend::DirectSound {
            #[cfg(target_os = "windows")]
            {
                let selected_directsound_name = selected_device_id
                    .and_then(|id| device_registry.find_available_by_id(id))
                    .filter(|endpoint| endpoint.backend == AudioBackend::DirectSound)
                    .map(|endpoint| endpoint.device_name.clone())
                    .or_else(|| selected_device.map(str::to_string));
                let mut engine = Self::new_directsound(selected_directsound_name)?;
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

        let host = host_for_backend(backend).map_err(|error| {
            error!(
                "Could not initialize {} audio host: {error:#}",
                backend.label()
            );
            error
        })?;
        let output_devices = Self::list_output_devices(&host);
        info!(
            "Enumerated {} output device(s) for {}: {:?}",
            output_devices.len(),
            backend.label(),
            output_devices
        );
        let out_dev = if let Some(selected_id) = selected_device_id {
            let parsed_id = selected_id.parse::<cpal::DeviceId>().ok();
            let by_id = parsed_id
                .as_ref()
                .and_then(|device_id| host.device_by_id(device_id));
            if let Some(device) = by_id {
                device
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
            host.output_devices()?
                .find(|device| device.to_string() == selected_name)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Selected {} output device '{}' is no longer available",
                        backend.label(),
                        selected_name
                    )
                })?
        } else {
            host.default_output_device()
                .or_else(|| {
                    host.output_devices()
                        .ok()
                        .and_then(|mut devices| devices.next())
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("No output audio device found for {}", backend.label())
                })?
        };
        // CPAL 0.18 removed DeviceTrait::name(); Display returns the user-facing device name.
        let out_name = out_dev.to_string();
        let out_id = Self::cpal_endpoint_id(backend, &out_dev, &out_name);

        // Native ASIO metadata is only needed for the selected TASCAM Computer
        // bus profile. Do not initialize every registered ASIO driver when the
        // active endpoint is a different vendor (e.g. Creative Sound Blaster).
        let normalized_selected_device = Self::normalize_device_name(&out_name);
        let needs_tascam_metadata = backend == AudioBackend::Asio
            && (normalized_selected_device.contains("tascam")
                || normalized_selected_device.contains("us322")
                || normalized_selected_device.contains("us366"));
        let probed_asio_channels = if needs_tascam_metadata {
            #[cfg(target_os = "windows")]
            {
                crate::audio::asio_probe::probe_registered_drivers()
            }
            #[cfg(not(target_os = "windows"))]
            {
                HashMap::new()
            }
        } else {
            HashMap::new()
        };
        let output_driver_channel_info =
            Self::match_asio_channel_info_to_devices(&output_devices, &probed_asio_channels);
        let (output_device_channels, output_device_channel_layouts) =
            Self::list_output_device_info(&host);
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
        let input_devices = Self::list_input_devices(&host);
        info!(
            "Enumerated {} input device(s) for {}: {:?}",
            input_devices.len(),
            backend.label(),
            input_devices
        );

        let output_config = preferred_output_config(&out_dev)
            .or_else(|| out_dev.default_output_config().ok())
            .ok_or_else(|| {
                anyhow::anyhow!("No supported output configuration found for {}", out_name)
            })?;
        let preferred_output_channels = output_config.channels() as usize;
        info!(
            "Selected output configuration for '{}': {} channels @ {} Hz, {:?}",
            out_name,
            preferred_output_channels,
            output_config.sample_rate(),
            output_config.sample_format()
        );
        let in_name = host
            .default_input_device()
            .map(|device| device.to_string())
            .unwrap_or_else(|| "Default Input".to_string());

        info!(
            "Audio backend: {}; Output: {}, Input: {}",
            backend.label(),
            out_name,
            in_name
        );

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
        let (ready_tx, ready_rx) = bounded::<Result<(usize, u32), String>>(1);

        std::thread::spawn(move || {
            audio_worker(
                backend,
                out_dev,
                output_config,
                worker_default_device,
                cmd_rx,
                pos_tx,
                worker_output_settings,
                ready_tx,
            );
        });

        let (output_channels, _actual_sample_rate) = ready_rx
            .recv_timeout(Duration::from_secs(15))
            .map_err(|error| {
                anyhow::anyhow!("Audio output initialization did not respond: {error}")
            })?
            .map_err(|error| anyhow::anyhow!("{error}"))?;
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
            output_device_channels,
            output_device_channel_layouts,
            output_settings_by_device,
            output_driver_channel_info,
            output_channel_names: HashMap::new(),
            default_output_pairs: HashMap::new(),
            output_channel_orders: HashMap::new(),
            detected_default_output_pairs,
        })
    }

    #[cfg(target_os = "windows")]
    fn new_directsound(selected_device: Option<String>) -> Result<Self> {
        let (cmd_tx, cmd_rx) = bounded::<AudioCommand>(256);
        let (pos_tx, pos_rx) = bounded::<PositionUpdate>(1024);
        let output_settings_by_device = Arc::new(RwLock::new(HashMap::new()));
        let worker_output_settings = output_settings_by_device.clone();
        let (ready_tx, ready_rx) = bounded::<Result<DirectSoundInitInfo, String>>(1);

        std::thread::spawn(move || {
            audio_worker_directsound(
                selected_device,
                cmd_rx,
                pos_tx,
                worker_output_settings,
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
            output_device_channels: ready.output_device_channels,
            output_device_channel_layouts: ready.output_device_channel_layouts,
            output_settings_by_device,
            output_driver_channel_info: HashMap::new(),
            output_channel_names: HashMap::new(),
            default_output_pairs: HashMap::new(),
            output_channel_orders: HashMap::new(),
            detected_default_output_pairs,
        })
    }

    fn normalize_device_name(value: &str) -> String {
        value
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .map(|character| character.to_ascii_lowercase())
            .collect()
    }

    fn match_asio_channel_info_to_devices(
        output_devices: &[String],
        probed: &HashMap<String, Vec<OutputChannelInfo>>,
    ) -> HashMap<String, Vec<OutputChannelInfo>> {
        let mut matched = HashMap::new();
        for device_name in output_devices {
            let normalized_device = Self::normalize_device_name(device_name);
            let best_match = probed
                .iter()
                .filter_map(|(driver_name, channels)| {
                    let normalized_driver = Self::normalize_device_name(driver_name);
                    let is_match = normalized_device == normalized_driver
                        || (normalized_device.len() >= 5
                            && normalized_driver.len() >= 5
                            && (normalized_device.contains(&normalized_driver)
                                || normalized_driver.contains(&normalized_device)));
                    is_match.then_some((normalized_driver.len(), channels))
                })
                .max_by_key(|(match_length, _)| *match_length);

            if let Some((_, channels)) = best_match {
                info!(
                    "Matched {} ASIO driver channel names to CPAL device '{}': {:?}",
                    channels.len(),
                    device_name,
                    channels
                        .iter()
                        .map(|channel| (&channel.name, channel.index))
                        .collect::<Vec<_>>()
                );
                matched.insert(device_name.clone(), channels.clone());
            } else {
                warn!(
                    "No ASIO channel-name metadata matched CPAL device '{}'; generic channel labels will be used",
                    device_name
                );
            }
        }
        matched
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

    /// Enumerate endpoint identities without opening streams or probing every
    /// device's channel configurations. The registry monitor calls this on a
    /// background thread; full channel capabilities are queried only when a
    /// backend becomes active or a user selects a route.
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
                            scan.endpoints.push(AudioEndpoint {
                                id,
                                backend,
                                device_name,
                                channels: 0,
                                channel_layouts: Vec::new(),
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
                    channels: 0,
                    channel_layouts: Vec::new(),
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

    fn output_device_config_key(&self, device_name: &str) -> String {
        format!("{}::{}", self.backend.label(), device_name)
    }

    /// Public key helper so AppSettings can persist calibration separately for
    /// the same hardware exposed through different backends.
    pub fn output_device_settings_key(&self, device_name: &str) -> String {
        self.output_device_config_key(device_name)
    }

    /// Default logical-to-native channel order for devices whose driver exposes
    /// channels in a different order from the device's mixer buses.
    ///
    /// The US-322/US-366 order below is based on the user's measured TASCAM
    /// Mixer Panel routing: ASIO 3/4 -> Computer 1/2, ASIO 5/6 -> Computer 3/4,
    /// ASIO 1/2 -> Computer 5/6. This is a presentation/routing profile only;
    /// samples are still written to the native stream channels unchanged.
    fn default_output_channel_order(&self, device_name: &str, channels: usize) -> Vec<usize> {
        Self::default_output_channel_order_for(self.backend, device_name, channels)
    }

    fn default_output_channel_order_for(
        backend: AudioBackend,
        device_name: &str,
        channels: usize,
    ) -> Vec<usize> {
        let normalized = Self::normalize_device_name(device_name);
        let is_tascam_us366 = backend == AudioBackend::Asio
            && (normalized.contains("us322") || normalized.contains("us366"));

        let mut order = if is_tascam_us366 && channels >= 6 {
            vec![2, 3, 4, 5, 0, 1]
        } else {
            Vec::new()
        };
        let mut used = order
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        for index in 0..channels {
            if used.insert(index) {
                order.push(index);
            }
        }
        order.truncate(channels);
        order
    }

    /// Display logical Theatre Show Cue outputs in their configured order. Values remain
    /// native zero-based stream indices, so existing cue routes keep their exact
    /// physical destination when a display/order profile is introduced.
    pub fn output_channel_display_order(&self, device_name: Option<&str>) -> Vec<usize> {
        let name = device_name.unwrap_or(self.current_device.as_str());
        let channels = self.output_channels_for_device(Some(name));
        let key = self.output_device_config_key(name);
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
            for index in self.default_output_channel_order(name, channels) {
                if used.insert(index) {
                    order.push(index);
                }
            }
        }

        // Be defensive with old or hand-edited settings: complete an incomplete
        // mapping with every native channel not already assigned.
        for index in 0..channels {
            if used.insert(index) {
                order.push(index);
            }
        }
        order
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

    /// Human-readable label for a zero-based stream channel index. User aliases
    /// override device-specific logical labels, which in turn override generic
    /// driver names. The native stream index is always retained for routing.
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

        let normalized_device = Self::normalize_device_name(name);
        let tascam_bus_name = if self.backend == AudioBackend::Asio
            && (normalized_device.contains("us322") || normalized_device.contains("us366"))
        {
            // Theatre Show Cue presents the TASCAM stream as six logical mono
            // destinations: 1L, 2R, 3L, 4R, 5L, 6R. The native ASIO order
            // remains 3, 4, 5, 6, 1, 2 on US-322/US-366.
            match channel_index {
                2 => Some("1L"),
                3 => Some("2R"),
                4 => Some("3L"),
                5 => Some("4R"),
                0 => Some("5L"),
                1 => Some("6R"),
                _ => None,
            }
        } else {
            None
        };

        let base = alias
            .or_else(|| tascam_bus_name.map(str::to_owned))
            .or(driver_name);

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

    pub fn detected_default_output_pair_for_device(
        &self,
        device_name: Option<&str>,
    ) -> Option<[usize; 2]> {
        let name = device_name.unwrap_or(self.current_device.as_str());
        self.detected_default_output_pairs
            .get(&self.output_device_config_key(name))
            .copied()
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

    pub fn default_output_pair_for_device(&self, device_name: Option<&str>) -> [usize; 2] {
        let name = device_name.unwrap_or(self.current_device.as_str());
        let key = self.output_device_config_key(name);
        self.default_output_pairs
            .get(&key)
            .copied()
            .or_else(|| self.detected_default_output_pairs.get(&key).copied())
            .unwrap_or_else(|| {
                let order = self.output_channel_display_order(Some(name));
                [
                    order.first().copied().unwrap_or(0),
                    order.get(1).copied().unwrap_or(1),
                ]
            })
    }

    pub fn set_default_output_pairs(&mut self, pairs: HashMap<String, [usize; 2]>) {
        self.default_output_pairs = pairs;
    }

    pub fn set_default_output_pair_for_device(&mut self, device_name: &str, pair: [usize; 2]) {
        let key = self.output_device_config_key(device_name);
        self.default_output_pairs.insert(key, pair);
    }

    /// Default stream indices for the first two source channels. A configured
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

    /// Default destinations for a source with the given channel count.
    /// Mono/stereo cues use the configured stereo pair; multichannel cues map
    /// channel-for-channel in Theatre Show Cue logical output order.
    pub fn default_route_channels_for_source(
        &self,
        device_name: Option<&str>,
        source_channels: usize,
    ) -> Vec<usize> {
        if source_channels <= 2 {
            self.default_route_output_channels(device_name)
        } else {
            self.output_channel_display_order(device_name)
                .into_iter()
                .take(source_channels)
                .collect()
        }
    }

    pub fn test_output_channel(
        &self,
        device_name: &str,
        logical_channel: usize,
        native_channel: usize,
    ) {
        let endpoint_id = if device_name == self.current_device.as_str() {
            self.current_device_id.as_str()
        } else {
            self.device_registry
                .find_available_by_backend_name(self.backend, device_name)
                .map(|endpoint| endpoint.id.as_str())
                .unwrap_or("unregistered")
        };
        info!(
            "Output test requested: backend='{}', endpoint_id='{}', device='{}', logical_output={}, native_channel={}",
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

    pub fn set_output_endpoint(&mut self, endpoint_id: &str, name: &str) -> Result<()> {
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
                reply: reply_tx,
            })
            .map_err(|error| {
                anyhow::anyhow!("Could not send output-device switch command: {error}")
            })?;

        let (channels, _sample_rate) = reply_rx
            .recv_timeout(Duration::from_secs(15))
            .map_err(|error| {
                anyhow::anyhow!("Audio worker did not confirm output-device switch: {error}")
            })?
            .map_err(|error| anyhow::anyhow!("{error}"))?;

        self.current_device = resolved_name.clone();
        self.current_device_id = endpoint_id.to_string();
        self.output_channels = channels.max(1);
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
        let mut current = shared.write();
        *current = settings;
        current.resize(channels, OutputChannelSetting::default());
        current.truncate(channels);
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

impl Drop for AudioEngine {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(AudioCommand::Shutdown);
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
    position_state: Arc<RwLock<f64>>,
    automation_update_tx: Sender<VolumeAutomationUpdate>,
    automation_update_rx: Receiver<VolumeAutomationUpdate>,
    fade_in_secs: f64,
    fade_out_secs: f64,
    fade_curve: FadeCurve,
    custom_fade_points: Vec<[f32; 2]>,
    channel_routes: Arc<RwLock<Vec<ChannelRoute>>>,
    output_gains: Arc<RwLock<Vec<f32>>>,
    output_channels: usize,
    output_device: String,
    uses_global_default: bool,
    mixer: rodio::mixer::Mixer,
    output_settings: SharedOutputSettings,
    loop_enabled: bool,
    loop_start: f64,
    loop_end: f64,
    loops_remaining: i32, // -1 = inf
    paused_at: Option<f64>,
    dynamic_fade: Option<DynamicFade>,
}

impl ActivePlayer {
    fn current_pos(&self) -> f64 {
        self.paused_at
            .unwrap_or_else(|| *self.position_state.read())
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
    start_secs: f64,
    channels: u64,
    sample_rate: f64,
    sample_index: u64,
    current_gain: f32,
    enabled: bool,
    points: Vec<[f64; 2]>,
    updates: Receiver<VolumeAutomationUpdate>,
}

impl<S> VolumeAutomationSource<S>
where
    S: Source,
{
    fn new(
        source: S,
        start_secs: f64,
        enabled: bool,
        points: Vec<[f64; 2]>,
        updates: Receiver<VolumeAutomationUpdate>,
    ) -> Self {
        let channels = source.channels().get().max(1) as u64;
        let sample_rate = source.sample_rate().get().max(1) as f64;
        Self {
            source,
            start_secs,
            channels,
            sample_rate,
            sample_index: 0,
            current_gain: 1.0,
            enabled,
            points,
            updates,
        }
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
        let frame_index = self.sample_index / self.channels;
        if channel_index == 0 {
            while let Ok(update) = self.updates.try_recv() {
                self.enabled = update.enabled;
                self.points = update.points;
            }
            let position_secs = self.start_secs + frame_index as f64 / self.sample_rate;
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
    passthrough_sample_index: u64,
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
    position_state: Arc<RwLock<f64>>,
}

impl<S> SpeedPitchSource<S>
where
    S: Source<Item = f32>,
{
    fn new(
        source: S,
        start_secs: f64,
        settings: Arc<RwLock<TimePitchSettings>>,
        position_state: Arc<RwLock<f64>>,
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
        *position_state.write() = start_secs.max(0.0);
        let passthrough = {
            let initial = settings.read();
            initial.speed_automation_enabled == false
                && initial.pitch_automation_enabled == false
                && (initial.speed - 1.0).abs() < 0.0001
                && initial.pitch_cents.abs() < 0.0001
        };
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
            passthrough_sample_index: 0,
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

    fn available_end_frame(&self) -> u64 {
        self.input_start_frame + (self.input_samples.len() / self.channels) as u64
    }

    fn fill_input_until(&mut self, required_end_frame: u64) {
        while !self.input_eof && self.available_end_frame() < required_end_frame {
            let mut frame = Vec::with_capacity(self.channels);
            for _ in 0..self.channels {
                match self.source.next() {
                    Some(sample) => frame.push(sample),
                    None => {
                        self.input_eof = true;
                        break;
                    }
                }
            }
            if frame.is_empty() {
                break;
            }
            frame.resize(self.channels, 0.0);
            self.input_samples.extend(frame);
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
            *self.position_state.write() = self.start_secs;
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

        let settings = self.settings.read().clone();
        let media_time = self.start_secs + self.input_cursor / self.sample_rate;
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
        let pitch_factor = 2.0_f32.powf(pitch_cents / 1200.0);
        let effective_speed = (speed
            * if settings.pitch_shift_changes_speed {
                pitch_factor
            } else {
                1.0
            })
        .clamp(0.1, 4.0);
        let effective_pitch = pitch_factor
            * if settings.preserve_pitch_on_speed_change {
                1.0
            } else {
                speed
            };
        let effective_pitch = effective_pitch.clamp(0.25, 4.0);
        let analysis_hop = self
            .previous_input_cursor
            .map(|previous| (self.input_cursor - previous).max(0.001))
            .unwrap_or(TIME_PITCH_HOP as f64 * effective_speed as f64);

        *self.position_state.write() = media_time;

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
                let settings = self.settings.read().clone();
                let still_neutral = !settings.speed_automation_enabled
                    && !settings.pitch_automation_enabled
                    && (settings.speed - 1.0).abs() < 0.0001
                    && settings.pitch_cents.abs() < 0.0001;
                if !still_neutral {
                    self.start_secs += self.passthrough_sample_index as f64
                        / self.channels as f64
                        / self.sample_rate;
                    self.passthrough_sample_index = 0;
                    self.input_start_frame = 0;
                    self.input_cursor = 0.0;
                    self.previous_input_cursor = None;
                    self.input_eof = false;
                    self.flush_remaining = 0;
                    self.finished = false;
                    self.phase_initialized = false;
                    *self.position_state.write() = self.start_secs;
                    self.passthrough = false;
                }
            }
            if self.passthrough {
                let sample = self.source.next();
                if sample.is_some() {
                    if self.passthrough_sample_index % self.channels as u64 == 0 {
                        let frame = self.passthrough_sample_index / self.channels as u64;
                        *self.position_state.write() =
                            self.start_secs + frame as f64 / self.sample_rate;
                    }
                    self.passthrough_sample_index = self.passthrough_sample_index.wrapping_add(1);
                } else {
                    let frames = self.passthrough_sample_index / self.channels as u64;
                    *self.position_state.write() =
                        self.start_secs + frames as f64 / self.sample_rate;
                    self.finished = true;
                }
                return sample;
            }
        }

        while self.output_samples.is_empty() && !self.finished {
            self.generate_hop();
        }
        let sample = self.output_samples.pop_front();
        if self.finished && self.output_samples.is_empty() {
            let final_position =
                self.start_secs + self.available_end_frame() as f64 / self.sample_rate;
            *self.position_state.write() = final_position;
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
}

impl<S> ChannelRouter<S>
where
    S: Source,
{
    fn new(
        source: S,
        routes: Arc<RwLock<Vec<ChannelRoute>>>,
        output_settings: SharedOutputSettings,
        output_gains: Arc<RwLock<Vec<f32>>>,
        output_channels: usize,
    ) -> Self {
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
            output_gains,
        }
    }

    fn mix_to_output(
        output_frame: &mut [f32],
        output_settings: &[OutputChannelSetting],
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
        let device_gain = setting
            .map(|setting| db_to_linear(setting.gain_db))
            .unwrap_or(1.0);
        let cue_gain = output_gains
            .get(output_index)
            .copied()
            .map(db_to_linear)
            .unwrap_or(1.0);
        output_frame[output_index] += sample * device_gain * cue_gain;
    }
}

impl<S> Iterator for ChannelRouter<S>
where
    S: Source,
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
            let output_gains = self.output_gains.read();
            for (source_index, sample) in self.input_frame.iter().copied().enumerate() {
                let fallback_output = source_index.min(self.output_channels - 1);
                let fallback_route = ChannelRoute {
                    active: true,
                    master_enabled: true,
                    output_channel: fallback_output,
                    output_channels: vec![fallback_output],
                    gain_db: 0.0,
                };
                let route = routes.get(source_index).unwrap_or(&fallback_route);
                if !route.active || !route.master_enabled {
                    continue;
                }

                let source_gain = db_to_linear(route.gain_db);
                if route.output_channels.is_empty() {
                    Self::mix_to_output(
                        &mut self.output_frame,
                        &outputs,
                        &output_gains,
                        route.output_channel,
                        sample * source_gain,
                    );
                } else {
                    for &output_index in &route.output_channels {
                        Self::mix_to_output(
                            &mut self.output_frame,
                            &outputs,
                            &output_gains,
                            output_index,
                            sample * source_gain,
                        );
                    }
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

fn open_sink_at(
    mixer: &rodio::mixer::Mixer,
    file_path: &PathBuf,
    offset_secs: f64,
    initial_vol: f32,
    output_channels: usize,
    routes: Arc<RwLock<Vec<ChannelRoute>>>,
    output_settings: SharedOutputSettings,
    output_gains: Arc<RwLock<Vec<f32>>>,
    automation_enabled: bool,
    automation_points: Vec<[f64; 2]>,
    automation_updates: Receiver<VolumeAutomationUpdate>,
    time_pitch: Arc<RwLock<TimePitchSettings>>,
    position_state: Arc<RwLock<f64>>,
) -> Option<rodio::Player> {
    let file = std::fs::File::open(file_path).ok()?;
    let player = rodio::Player::connect_new(mixer);
    let decoder = rodio::Decoder::try_from(file).ok()?;
    let source = decoder.skip_duration(Duration::from_secs_f64(offset_secs.max(0.0)));
    let automated = VolumeAutomationSource::new(
        source,
        offset_secs.max(0.0),
        automation_enabled,
        automation_points,
        automation_updates,
    );
    let transformed =
        SpeedPitchSource::new(automated, offset_secs.max(0.0), time_pitch, position_state);
    let routed = ChannelRouter::new(
        transformed,
        routes,
        output_settings,
        output_gains,
        output_channels,
    );
    player.set_volume(initial_vol);
    player.append(routed);
    player.play();
    Some(player)
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
const DIRECTSOUND_QUEUE_TARGET_FRAMES: usize = 2_048;
#[cfg(target_os = "windows")]
const DIRECTSOUND_QUEUE_LOW_WATERMARK_FRAMES: usize = 1_024;

#[allow(dead_code)]
enum OutputStreamHandle {
    Cpal(rodio::MixerDeviceSink),
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
}

#[cfg(target_os = "windows")]
impl DirectSoundQueue {
    fn open(
        device_name: Option<&str>,
        channels: u8,
        sample_rate: u32,
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
            samples: 1024,
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
        unsafe {
            sdl2::sys::SDL_CloseAudioDevice(self.device_id);
        }
    }
}

#[cfg(target_os = "windows")]
struct DirectSoundInitInfo {
    output_devices: Vec<String>,
    current_device: String,
    output_channels: usize,
    output_device_channels: HashMap<String, usize>,
    output_device_channel_layouts: HashMap<String, Vec<usize>>,
}

struct DeviceOutput {
    // Keeping the native stream/device alive is required for audio playback.
    _stream: OutputStreamHandle,
    mixer: rodio::mixer::Mixer,
    channels: usize,
    sample_rate: u32,
    settings: SharedOutputSettings,
    #[cfg(target_os = "windows")]
    directsound_source: Option<rodio::mixer::MixerSource>,
}

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
    device: &Device,
    preferred: cpal::SupportedStreamConfig,
) -> Vec<cpal::SupportedStreamConfig> {
    let mut candidates = vec![preferred];

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
    device: &Device,
    preferred_config: cpal::SupportedStreamConfig,
    device_name: &str,
    settings_by_device: &OutputSettingsByDevice,
) -> Option<DeviceOutput> {
    let candidates = output_config_candidates(device, preferred_config);
    let mut failures = Vec::new();

    for config in candidates {
        let channels = (config.channels() as usize).max(1);
        let sample_rate = config.sample_rate();
        let sample_format = config.sample_format();

        debug!(
            "Trying output '{}' with {} channels @ {} Hz ({:?})",
            device_name, channels, sample_rate, sample_format
        );

        let mut stream = match rodio::DeviceSinkBuilder::default()
            .with_device(device.clone())
            .with_supported_config(&config)
            .open_stream()
        {
            Ok(stream) => stream,
            Err(open_error) => {
                failures.push(format!(
                    "{}ch @ {}Hz {:?}: {}",
                    channels, sample_rate, sample_format, open_error
                ));
                continue;
            }
        };

        info!(
            "Audio output stream opened successfully: '{}' ({} channels @ {} Hz, {:?})",
            device_name, channels, sample_rate, sample_format
        );
        stream.log_on_drop(false);
        let mixer = stream.mixer().clone();
        let settings = shared_output_settings(settings_by_device, device_name, channels);
        return Some(DeviceOutput {
            _stream: OutputStreamHandle::Cpal(stream),
            mixer,
            channels,
            sample_rate,
            settings,
            #[cfg(target_os = "windows")]
            directsound_source: None,
        });
    }

    warn!(
        "Could not open audio output '{}'. Tried {} supported configurations. Details: {}",
        device_name,
        failures.len(),
        failures.join(" | ")
    );
    None
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
) -> Option<DeviceOutput> {
    use std::num::{NonZeroU16, NonZeroU32};

    let (channels, preferred_rate) = directsound_device_spec(audio, device_name);
    let channels_u8 = u8::try_from(channels).ok().filter(|value| *value > 0)?;
    let selected_device = (device_name != DIRECTSOUND_DEFAULT_DEVICE).then_some(device_name);

    let mut rates = Vec::new();
    for rate in [preferred_rate, 48_000, 44_100, 96_000] {
        if !rates.contains(&rate) {
            rates.push(rate);
        }
    }

    let mut last_error = None;
    for sample_rate in rates {
        let queue = match DirectSoundQueue::open(selected_device, channels_u8, sample_rate) {
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

        // Prime the queue with silence before resuming playback.
        let initial_silence = vec![0.0_f32; actual_channels * DIRECTSOUND_QUEUE_TARGET_FRAMES];
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
            "DirectSound output opened: '{}' ({} channels @ {} Hz; float32 queue; SDL driver: directsound)",
            device_name, actual_channels, actual_sample_rate
        );

        let settings = shared_output_settings(settings_by_device, device_name, actual_channels);
        return Some(DeviceOutput {
            _stream: OutputStreamHandle::DirectSound(queue),
            mixer,
            channels: actual_channels,
            sample_rate: actual_sample_rate,
            settings,
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
    device_name: &str,
    context: &AudioOutputContext,
    settings_by_device: &OutputSettingsByDevice,
) -> Option<DeviceOutput> {
    match context {
        AudioOutputContext::Cpal(host) => {
            let devices = host.output_devices().ok()?;
            let device = devices
                .into_iter()
                .find(|device| device.to_string() == device_name)?;
            let config =
                preferred_output_config(&device).or_else(|| device.default_output_config().ok())?;
            build_device_output(&device, config, device_name, settings_by_device)
        }
        #[cfg(target_os = "windows")]
        AudioOutputContext::DirectSound { audio, .. } => {
            build_directsound_device_output(audio, device_name, settings_by_device)
        }
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
        if queued_frames >= DIRECTSOUND_QUEUE_LOW_WATERMARK_FRAMES {
            continue;
        }

        let frames_to_queue = DIRECTSOUND_QUEUE_TARGET_FRAMES.saturating_sub(queued_frames);
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
    streams: &mut HashMap<String, DeviceOutput>,
    device_name: &str,
    context: &AudioOutputContext,
    settings_by_device: &OutputSettingsByDevice,
) -> Option<(rodio::mixer::Mixer, usize, u32, SharedOutputSettings)> {
    if !streams.contains_key(device_name) {
        let output = open_device_output(device_name, context, settings_by_device)?;
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
    initial_device: Device,
    initial_config: cpal::SupportedStreamConfig,
    initial_device_name: String,
    cmd_rx: Receiver<AudioCommand>,
    pos_tx: Sender<PositionUpdate>,
    output_settings_by_device: OutputSettingsByDevice,
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
    let Some(initial_output) = build_device_output(
        &initial_device,
        initial_config,
        &initial_device_name,
        &output_settings_by_device,
    ) else {
        error!(
            "Could not open initial {} audio output stream",
            backend.label()
        );
        let _ = ready_tx.send(Err(format!(
            "Could not open initial {} audio output stream for '{}'",
            backend.label(),
            initial_device_name
        )));
        return;
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
    );
}

#[cfg(target_os = "windows")]
fn audio_worker_directsound(
    selected_device: Option<String>,
    cmd_rx: Receiver<AudioCommand>,
    pos_tx: Sender<PositionUpdate>,
    output_settings_by_device: OutputSettingsByDevice,
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

        let output_devices = list_directsound_devices(&audio);
        let current_device = match selected_device {
            Some(selected) => output_devices
                .iter()
                .find(|device| {
                    device.as_str() == selected
                        || AudioEngine::normalize_device_name(device.as_str())
                            == AudioEngine::normalize_device_name(&selected)
                })
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Selected DirectSound output device '{}' is no longer available",
                        selected
                    )
                })?,
            None => DIRECTSOUND_DEFAULT_DEVICE.to_string(),
        };
        let initial_output =
            build_directsound_device_output(&audio, &current_device, &output_settings_by_device)
                .ok_or_else(|| anyhow::anyhow!("Could not open the default DirectSound output"))?;

        let mut output_device_channels = HashMap::new();
        let mut output_device_channel_layouts = HashMap::new();
        for name in &output_devices {
            let (channels, _) = directsound_device_spec(&audio, name);
            output_device_channels.insert(name.clone(), channels.max(1));
            output_device_channel_layouts.insert(name.clone(), vec![channels.max(1)]);
        }

        let ready = DirectSoundInitInfo {
            output_devices,
            current_device: current_device.clone(),
            output_channels: initial_output.channels,
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
            );
        }
        Err(error) => {
            error!("Could not initialize DirectSound output: {error:#}");
            let _ = ready_tx.send(Err(format!("{error:#}")));
        }
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
) {
    let mut streams = HashMap::new();
    streams.insert(initial_device_name.clone(), initial_output);
    let mut default_device_name = initial_device_name;
    let mut players: Vec<ActivePlayer> = Vec::new();
    let test_generation = Arc::new(AtomicU64::new(0));
    let tick = Duration::from_millis(5);

    loop {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                AudioCommand::Shutdown => {
                    for player in &players {
                        player.sink.stop();
                    }
                    players.clear();
                    test_generation.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                AudioCommand::SetOutputDevice {
                    device_name: dev_name,
                    reply,
                } => {
                    let previous_default_device = default_device_name.clone();
                    test_generation.fetch_add(1, Ordering::Relaxed);

                    if backend == AudioBackend::Asio && dev_name != previous_default_device {
                        // CPAL's ASIO host owns one driver at a time per process.
                        // Stop all players and drop every stream before opening
                        // another ASIO device; Stage 4 will isolate drivers in processes.
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
                        &mut streams,
                        &dev_name,
                        &context,
                        &output_settings_by_device,
                    ) {
                        let mut current = settings.write();
                        current.resize(channels, OutputChannelSetting::default());
                        current.truncate(channels);
                        info!(
                            "Switched global output device to: {} — {} ({} channels @ {} Hz)",
                            backend.short_label(),
                            dev_name,
                            channels,
                            sample_rate
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
                                &mut streams,
                                &previous_default_device,
                                &context,
                                &output_settings_by_device,
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
                other => handle_command(
                    other,
                    backend,
                    &mut players,
                    &mut streams,
                    &context,
                    &default_device_name,
                    &output_settings_by_device,
                    &test_generation,
                ),
            }
        }

        let mut finished_ids: Vec<(Uuid, u64)> = Vec::new();
        for player in &mut players {
            if player.sink.is_paused() {
                let _ = pos_tx.try_send(PositionUpdate {
                    id: player.id,
                    generation: player.generation,
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
                        &player.mixer,
                        &player.file,
                        player.loop_start,
                        player.current_volume,
                        player.output_channels,
                        player.channel_routes.clone(),
                        player.output_settings.clone(),
                        player.output_gains.clone(),
                        player.volume_automation_enabled,
                        player.volume_automation.clone(),
                        player.automation_update_rx.clone(),
                        player.time_pitch.clone(),
                        player.position_state.clone(),
                    ) {
                        player.sink = new_sink;
                        pos = player.loop_start;
                    } else {
                        finished_ids.push((player.id, player.generation));
                        continue;
                    }
                }
            }

            if (player.end_secs > 0.0 && pos >= player.end_secs) || player.sink.empty() {
                player.sink.stop();
                finished_ids.push((player.id, player.generation));
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
                        finished_ids.push((player.id, player.generation));
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

        for (finished_id, generation) in finished_ids {
            players.retain(|player| player.id != finished_id || player.generation != generation);
            let _ = pos_tx.try_send(PositionUpdate {
                id: finished_id,
                generation,
                position_secs: 0.0,
                state: CueState::Stopped,
            });
        }

        #[cfg(target_os = "windows")]
        pump_directsound_queues(&mut streams);

        std::thread::sleep(tick);
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
    test_generation: &Arc<AtomicU64>,
) {
    match cmd {
        AudioCommand::Play {
            id,
            generation,
            file,
            output_device,
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
                // Older restart tails are already near silence. Stop those and
                // retain only the newest instance for one clean crossfade tail.
                if Some(player.generation) != previous_generation
                    || player.sink.is_paused()
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
            let Some((mixer, output_channels, _sample_rate, output_settings)) =
                ensure_device_output(streams, &device_name, context, output_settings_by_device)
            else {
                warn!("Cannot open output device '{}' for cue {}", device_name, id);
                return;
            };

            info!(
                "CUE OUTPUT ROUTING: cue={}, device='{}', stream_channels={}, routes={:?}",
                id, device_name, output_channels, channel_routes
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
            let position_state = Arc::new(RwLock::new(start_secs.max(0.0)));
            let Some(sink) = open_sink_at(
                &mixer,
                &file,
                start_secs,
                initial_volume,
                output_channels,
                route_state.clone(),
                output_settings.clone(),
                output_gains_state.clone(),
                volume_automation_enabled,
                volume_automation.clone(),
                automation_update_rx.clone(),
                time_pitch_state.clone(),
                position_state.clone(),
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
                automation_update_tx,
                automation_update_rx,
                fade_in_secs: effective_fade_in_secs,
                fade_out_secs,
                fade_curve,
                custom_fade_points,
                channel_routes: route_state,
                output_gains: output_gains_state,
                output_channels,
                output_device: device_name,
                uses_global_default,
                mixer,
                output_settings,
                loop_enabled,
                loop_start,
                loop_end: effective_loop_end,
                loops_remaining: loop_count,
                paused_at: None,
                dynamic_fade: None,
            });
        }

        AudioCommand::Stop(id) => {
            for player in players.iter().filter(|player| player.id == id) {
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
            if let Some(player) = players.iter_mut().rev().find(|player| player.id == id) {
                let position = player.current_pos();
                player.sink.pause();
                player.paused_at = Some(position);
            }
        }

        AudioCommand::Seek { id, position_secs } => {
            if let Some(player) = players.iter_mut().rev().find(|player| player.id == id) {
                let min_position = player.start_secs.max(0.0);
                let max_position = if player.end_secs > 0.0 {
                    (player.end_secs - 0.01).max(min_position)
                } else {
                    f64::MAX
                };
                let position = position_secs.clamp(min_position, max_position);
                player.sink.stop();
                if let Some(new_sink) = open_sink_at(
                    &player.mixer,
                    &player.file,
                    position,
                    player.current_volume,
                    player.output_channels,
                    player.channel_routes.clone(),
                    player.output_settings.clone(),
                    player.output_gains.clone(),
                    player.volume_automation_enabled,
                    player.volume_automation.clone(),
                    player.automation_update_rx.clone(),
                    player.time_pitch.clone(),
                    player.position_state.clone(),
                ) {
                    new_sink.pause();
                    player.sink = new_sink;
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
            if let Some(player) = players.iter_mut().rev().find(|player| player.id == id) {
                player.paused_at = None;
                player.sink.play();
            }
        }

        AudioCommand::ResumeAll => {
            for player in players.iter_mut() {
                player.paused_at = None;
                player.sink.play();
            }
        }

        AudioCommand::SetVolume(id, volume) => {
            if let Some(player) = players.iter_mut().rev().find(|player| player.id == id) {
                player.base_volume = volume;
                player.sink.set_volume(volume);
            }
        }

        AudioCommand::SetVolumeAutomation {
            id,
            enabled,
            points,
        } => {
            if let Some(player) = players.iter_mut().rev().find(|player| player.id == id) {
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
            if let Some(player) = players.iter_mut().rev().find(|player| player.id == id) {
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
            if let Some(player) = players.iter_mut().rev().find(|player| player.id == id) {
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
            if let Some(player) = players.iter_mut().rev().find(|player| player.id == id) {
                *player.channel_routes.write() = routes;
            }
        }

        AudioCommand::SetOutputGains { id, gains_db } => {
            if let Some(player) = players.iter_mut().rev().find(|player| player.id == id) {
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
            let Some((mixer, output_channels, sample_rate, _)) =
                ensure_device_output(streams, &device_name, context, output_settings_by_device)
            else {
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
        AudioCommand::SetOutputDevice { .. }
        | AudioCommand::SetInputDevice(_)
        | AudioCommand::Shutdown => {}
    }
}

#[cfg(test)]
mod channel_router_tests {
    use super::*;
    use rodio::buffer::SamplesBuffer;

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
        let mut automated =
            VolumeAutomationSource::new(source, 0.0, true, vec![[0.0, 1.0], [1.0, 0.0]], rx);
        let output = (0..6)
            .map(|_| automated.next().expect("automation sample"))
            .collect::<Vec<_>>();
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
        let mut automated = VolumeAutomationSource::new(source, 0.0, false, Vec::new(), rx);
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
    fn tascam_us366_logical_outputs_follow_computer_bus_order() {
        let order =
            AudioEngine::default_output_channel_order_for(AudioBackend::Asio, "US-322 / US-366", 6);
        assert_eq!(order, vec![2, 3, 4, 5, 0, 1]);
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
    fn detects_tascam_physical_stereo_pair_from_driver_channel_names() {
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
        let position = Arc::new(RwLock::new(0.0));
        let output: Vec<f32> = SpeedPitchSource::new(source, 0.0, settings, position).collect();
        assert_eq!(output, samples);
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
        let position = Arc::new(RwLock::new(0.0));
        let output: Vec<f32> = SpeedPitchSource::new(source, 0.0, settings, position).collect();
        assert!(!output.is_empty());
        assert!(output.iter().all(|sample| sample.is_finite()));
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
