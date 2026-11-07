// src/audio/engine.rs — аудиодвижок с поддержкой Fade In/Out, Start/End, Loop и смены I/O карт

use crate::audio::OutputChannelInfo;
use crate::config::AudioBackend;
use crate::cue::{ChannelRoute, CueState, FadeCurve, OutputChannelSetting};
use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, Host};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
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
    SetVolumeAutomation {
        id: Uuid,
        enabled: bool,
        points: Vec<[f64; 2]>,
    },
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
    /// Play a raw multichannel test tone through one native stream channel.
    /// logical_channel is the user-facing CueLabs output slot; channel is the
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
    #[cfg(all(target_os = "windows", feature = "asio"))]
    {
        let host_id = match backend {
            AudioBackend::Wasapi => cpal::HostId::Wasapi,
            AudioBackend::Asio => cpal::HostId::Asio,
        };
        return cpal::host_from_id(host_id).map_err(|error| {
            anyhow::anyhow!(
                "Could not initialise {} audio host: {}",
                backend.label(),
                error
            )
        });
    }
    #[cfg(all(target_os = "windows", not(feature = "asio")))]
    {
        match backend {
            AudioBackend::Wasapi => Ok(cpal::host_from_id(cpal::HostId::Wasapi)
                .map_err(|error| anyhow::anyhow!("Could not initialise WASAPI host: {}", error))?),
            AudioBackend::Asio => Err(anyhow::anyhow!(
                "ASIO support is not compiled in. Rebuild CueLabs with `--features asio`."
            )),
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        match backend {
            AudioBackend::Wasapi => Ok(cpal::default_host()),
            AudioBackend::Asio => Err(anyhow::anyhow!("ASIO backend is only available on Windows")),
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
    pub input_devices: Vec<String>,
    pub current_device: String,
    pub current_input_device: String,
    pub output_channels: usize,
    output_device_channels: HashMap<String, usize>,
    output_device_channel_layouts: HashMap<String, Vec<usize>>,
    output_channel_settings: Arc<RwLock<Vec<OutputChannelSetting>>>,
    /// Optional channel names reported by the native ASIO driver, keyed by CPAL device name.
    output_driver_channel_info: HashMap<String, Vec<OutputChannelInfo>>,
    /// User-defined aliases override driver-provided names but never change routing indices.
    output_channel_names: HashMap<String, Vec<String>>,
    default_output_pairs: HashMap<String, [usize; 2]>,
    /// Logical CueLabs output order -> native CPAL/ASIO stream channel index.
    /// Stored per device so non-standard driver orders do not affect other interfaces.
    output_channel_orders: HashMap<String, Vec<usize>>,
    detected_default_output_pairs: HashMap<String, [usize; 2]>,
}

impl AudioEngine {
    pub fn new() -> Result<Self> {
        Self::new_with_backend(AudioBackend::Wasapi)
    }

    pub fn new_with_backend(backend: AudioBackend) -> Result<Self> {
        // Query native ASIO channel names before CPAL initializes the driver.
        // CPAL currently exposes only a channel count, not ASIOGetChannelInfo metadata.
        let probed_asio_channels = if backend == AudioBackend::Asio {
            #[cfg(all(target_os = "windows", feature = "asio"))]
            {
                crate::audio::asio_probe::probe_registered_drivers()
            }
            #[cfg(not(all(target_os = "windows", feature = "asio")))]
            {
                HashMap::new()
            }
        } else {
            HashMap::new()
        };

        let host = host_for_backend(backend)?;
        let output_devices = Self::list_output_devices(&host);
        let output_driver_channel_info =
            Self::match_asio_channel_info_to_devices(&output_devices, &probed_asio_channels);
        let (output_device_channels, output_device_channel_layouts) =
            Self::list_output_device_info(&host);
        let input_devices = Self::list_input_devices(&host);

        let out_dev = host
            .default_output_device()
            .or_else(|| {
                host.output_devices()
                    .ok()
                    .and_then(|mut devices| devices.next())
            })
            .ok_or_else(|| {
                anyhow::anyhow!("No output audio device found for {}", backend.label())
            })?;
        // CPAL 0.18 removed DeviceTrait::name(); Display returns the user-facing device name.
        let out_name = out_dev.to_string();
        let output_config = preferred_output_config(&out_dev)
            .or_else(|| out_dev.default_output_config().ok())
            .ok_or_else(|| {
                anyhow::anyhow!("No supported output configuration found for {}", out_name)
            })?;
        let output_channels = output_config.channels() as usize;

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
        let output_channel_settings = Arc::new(RwLock::new(vec![
            OutputChannelSetting::default();
            output_channels.max(1)
        ]));
        let worker_output_settings = output_channel_settings.clone();
        let worker_default_device = out_name.clone();

        std::thread::spawn(move || {
            audio_worker(
                backend,
                out_dev,
                output_config,
                worker_default_device,
                cmd_rx,
                pos_tx,
                worker_output_settings,
            );
        });

        let detected_default_output_pairs =
            Self::detect_default_pairs_for_devices(backend, &output_driver_channel_info);

        Ok(Self {
            backend,
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
            output_driver_channel_info,
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
    /// after CueLabs started.
    pub fn refresh_output_device_info(&mut self, device_name: &str) {
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

    /// Display logical CueLabs outputs in their configured order. Values remain
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

    /// Human-readable label for a zero-based stream channel index. ASIO driver
    /// names are preferred, while user aliases override them. The actual
    /// zero-based stream index is always retained for routing.
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
            match channel_index {
                2 => Some("Computer 1/L"),
                3 => Some("Computer 1/R"),
                4 => Some("Computer 3/L"),
                5 => Some("Computer 3/R"),
                0 => Some("Computer 5/L"),
                1 => Some("Computer 5/R"),
                _ => None,
            }
        } else {
            None
        };

        let base = alias
            .or(driver_name)
            .or_else(|| tascam_bus_name.map(str::to_owned));

        match (self.backend, base) {
            (AudioBackend::Asio, Some(label)) => {
                format!("{} · ASIO {}", label, channel_index + 1)
            }
            (AudioBackend::Asio, None) => format!(
                "CueLabs Output {} · ASIO {}",
                logical_index + 1,
                channel_index + 1
            ),
            (_, Some(label)) => format!("{} · Ch. {}", label, channel_index + 1),
            (_, None) => format!(
                "CueLabs Output {} · Ch. {}",
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

    pub fn test_output_channel(
        &self,
        device_name: &str,
        logical_channel: usize,
        native_channel: usize,
    ) {
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
    automation_update_tx: Sender<VolumeAutomationUpdate>,
    automation_update_rx: Receiver<VolumeAutomationUpdate>,
    fade_in_secs: f64,
    fade_out_secs: f64,
    fade_curve: FadeCurve,
    custom_fade_points: Vec<[f32; 2]>,
    channel_routes: Arc<RwLock<Vec<ChannelRoute>>>,
    output_channels: usize,
    output_device: String,
    uses_global_default: bool,
    mixer: rodio::mixer::Mixer,
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
    S: Source,
{
    fn new(
        source: S,
        routes: Arc<RwLock<Vec<ChannelRoute>>>,
        output_settings: Arc<RwLock<Vec<OutputChannelSetting>>>,
        output_channels: usize,
    ) -> Self {
        let input_channels = source.channels().get().max(1) as usize;
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
    output_settings: Arc<RwLock<Vec<OutputChannelSetting>>>,
    automation_enabled: bool,
    automation_points: Vec<[f64; 2]>,
    automation_updates: Receiver<VolumeAutomationUpdate>,
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
    let routed = ChannelRouter::new(automated, routes, output_settings, output_channels);
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
}

impl Iterator for RawOutputTestTone {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        let channels = self.output_channels.max(1) as u64;
        let channel = (self.sample_index % channels) as usize;
        let frame = self.sample_index / channels;
        self.sample_index = self.sample_index.wrapping_add(1);

        if channel != self.selected_channel {
            return Some(0.0);
        }

        let phase = std::f32::consts::TAU * 440.0 * frame as f32 / self.sample_rate.max(1) as f32;
        Some(phase.sin() * 0.12)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

impl Source for RawOutputTestTone {
    fn current_span_len(&self) -> Option<usize> {
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

struct DeviceOutput {
    // Keeping the stream alive is required for the device's output callback.
    _stream: rodio::MixerDeviceSink,
    mixer: rodio::mixer::Mixer,
    channels: usize,
    sample_rate: u32,
    settings: Arc<RwLock<Vec<OutputChannelSetting>>>,
}

fn build_device_output(
    device: &Device,
    config: cpal::SupportedStreamConfig,
    device_name: &str,
    default_device_name: &str,
    default_settings: &Arc<RwLock<Vec<OutputChannelSetting>>>,
) -> Option<DeviceOutput> {
    let channels = (config.channels() as usize).max(1);
    let sample_rate = config.sample_rate();
    let sample_format = config.sample_format();
    info!(
        "Opening output '{}' with {} channels @ {} Hz ({:?})",
        device_name, channels, sample_rate, sample_format
    );
    let mut stream = match rodio::DeviceSinkBuilder::default()
        .with_device(device.clone())
        .with_supported_config(&config)
        .open_stream()
    {
        Ok(stream) => stream,
        Err(open_error) => {
            error!(
                "Could not open audio output '{}' ({} channels @ {} Hz): {}",
                device_name, channels, sample_rate, open_error
            );
            return None;
        }
    };
    info!(
        "Audio output stream opened successfully: '{}' ({} channels @ {} Hz)",
        device_name, channels, sample_rate
    );
    stream.log_on_drop(false);
    let mixer = stream.mixer().clone();
    let settings = if device_name == default_device_name {
        default_settings.clone()
    } else {
        Arc::new(RwLock::new(vec![OutputChannelSetting::default(); channels]))
    };
    Some(DeviceOutput {
        _stream: stream,
        mixer,
        channels,
        sample_rate,
        settings,
    })
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
        .find(|device| device.to_string() == device_name)?;
    let config =
        preferred_output_config(&device).or_else(|| device.default_output_config().ok())?;
    build_device_output(
        &device,
        config,
        device_name,
        default_device_name,
        default_settings,
    )
}

fn ensure_device_output(
    streams: &mut HashMap<String, DeviceOutput>,
    device_name: &str,
    host: &Host,
    default_device_name: &str,
    default_settings: &Arc<RwLock<Vec<OutputChannelSetting>>>,
) -> Option<(
    rodio::mixer::Mixer,
    usize,
    u32,
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
    output_settings: Arc<RwLock<Vec<OutputChannelSetting>>>,
) {
    let host = match host_for_backend(backend) {
        Ok(host) => host,
        Err(error) => {
            error!(
                "Could not initialise {} audio host in worker: {}",
                backend.label(),
                error
            );
            return;
        }
    };
    let Some(initial_output) = build_device_output(
        &initial_device,
        initial_config,
        &initial_device_name,
        &initial_device_name,
        &output_settings,
    ) else {
        error!(
            "Could not open initial {} audio output stream",
            backend.label()
        );
        return;
    };

    let mut streams = HashMap::new();
    streams.insert(initial_device_name.clone(), initial_output);
    let mut default_device_name = initial_device_name;
    let mut players: Vec<ActivePlayer> = Vec::new();
    let mut test_players: Vec<(rodio::Player, Instant)> = Vec::new();
    let tick = Duration::from_millis(5);

    loop {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                AudioCommand::Shutdown => {
                    for player in &players {
                        player.sink.stop();
                    }
                    players.clear();
                    for (player, _) in &test_players {
                        player.stop();
                    }
                    test_players.clear();
                    return;
                }
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
                    if let Some((_, channels, _, settings)) = ensure_device_output(
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
                    &mut test_players,
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
                        player.volume_automation_enabled,
                        player.volume_automation.clone(),
                        player.automation_update_rx.clone(),
                    ) {
                        player.sink = new_sink;
                        player.segment_start_pos = player.loop_start;
                        player.segment_instant = Instant::now();
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

        let now = Instant::now();
        test_players.retain(|(player, stop_at)| {
            if now >= *stop_at {
                player.stop();
                false
            } else {
                true
            }
        });

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
    test_players: &mut Vec<(rodio::Player, Instant)>,
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
            volume_automation.retain(|point| point[0].is_finite() && point[1].is_finite());
            volume_automation.sort_by(|a, b| a[0].total_cmp(&b[0]));
            for point in &mut volume_automation {
                point[0] = point[0].clamp(0.0, duration_secs.max(0.01));
                point[1] = point[1].clamp(0.0, 1.0);
            }
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
            let Some((mixer, output_channels, _sample_rate, output_settings)) =
                ensure_device_output(
                    streams,
                    &device_name,
                    host,
                    default_device_name,
                    default_settings,
                )
            else {
                warn!("Cannot open output device '{}' for cue {}", device_name, id);
                return;
            };

            info!(
                "CUE OUTPUT ROUTING: cue={}, device='{}', stream_channels={}, routes={:?}",
                id, device_name, output_channels, channel_routes
            );
            let initial_volume = if fade_in_secs > 0.0 { 0.0 } else { volume };
            let route_state = Arc::new(RwLock::new(channel_routes));
            let (automation_update_tx, automation_update_rx) = unbounded();
            let Some(sink) = open_sink_at(
                &mixer,
                &file,
                start_secs,
                initial_volume,
                output_channels,
                route_state.clone(),
                output_settings.clone(),
                volume_automation_enabled,
                volume_automation.clone(),
                automation_update_rx.clone(),
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
                automation_update_tx,
                automation_update_rx,
                fade_in_secs,
                fade_out_secs,
                fade_curve,
                custom_fade_points,
                channel_routes: route_state,
                output_channels,
                output_device: device_name,
                uses_global_default,
                mixer,
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
                    &player.mixer,
                    &player.file,
                    position,
                    player.current_volume,
                    player.output_channels,
                    player.channel_routes.clone(),
                    player.output_settings.clone(),
                    player.volume_automation_enabled,
                    player.volume_automation.clone(),
                    player.automation_update_rx.clone(),
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

        AudioCommand::SetVolumeAutomation {
            id,
            enabled,
            points,
        } => {
            if let Some(player) = players.iter_mut().find(|player| player.id == id) {
                player.volume_automation_enabled = enabled;
                player.volume_automation = points.clone();
                // The source consumes updates at audio-frame boundaries without blocking.
                let _ = player
                    .automation_update_tx
                    .send(VolumeAutomationUpdate { enabled, points });
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

        AudioCommand::TestOutput {
            device_name,
            logical_channel,
            channel,
        } => {
            for (previous_test, _) in test_players.drain(..) {
                previous_test.stop();
            }

            // Use the exact device selected in Settings, not an implicitly captured
            // global default. This matters when a cue or the UI selects a non-default
            // ASIO endpoint.
            let Some((mixer, output_channels, sample_rate, _)) = ensure_device_output(
                streams,
                &device_name,
                host,
                default_device_name,
                default_settings,
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

            // IMPORTANT: use a continuous source instead of a finite SamplesBuffer.
            // Rodio's Mixer wraps every source in UniformSourceIterator, which caps
            // finite spans at 32,768 samples. For 6 channels that cap ends 2 samples
            // into the next frame and can rotate the channel phase on the next span.
            // This source reports a stable format until stopped by the test timer.
            let source = RawOutputTestTone {
                output_channels,
                selected_channel: channel,
                sample_rate,
                sample_index: 0,
            };

            let player = rodio::Player::connect_new(&mixer);
            player.set_volume(1.0);
            player.append(source);
            player.play();
            test_players.push((player, Instant::now() + Duration::from_millis(900)));
            info!(
                "OUTPUT TEST: device='{}', CueLabs logical output {} -> native CPAL/ASIO stream channel {} (zero-based index {}), stream={}ch @ {}Hz; ChannelRouter bypassed; continuous frame-aligned source",
                device_name,
                logical_channel + 1,
                channel + 1,
                channel,
                output_channels,
                sample_rate
            );
        }
        AudioCommand::SetOutputDevice(_)
        | AudioCommand::SetInputDevice(_)
        | AudioCommand::Shutdown => {}
    }
}

#[cfg(test)]
mod channel_router_tests {
    use super::*;
    use rodio::buffer::SamplesBuffer;

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

        let source = RawOutputTestTone {
            output_channels: 6,
            selected_channel: 4,
            sample_rate: 48_000,
            sample_index: 0,
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
    fn channel_router_keeps_six_channel_frame_alignment_across_rodio_spans() {
        use rodio::source::UniformSourceIterator;

        let routes = Arc::new(RwLock::new(vec![
            ChannelRoute {
                active: true,
                output_channel: 4,
                gain_db: 0.0,
            },
            ChannelRoute {
                active: true,
                output_channel: 5,
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
        let router = ChannelRouter::new(source, routes, output_settings, 6);
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
        let source = SamplesBuffer::new(
            std::num::NonZeroU16::new(2).unwrap(),
            std::num::NonZeroU32::new(48_000).unwrap(),
            vec![0.25_f32, 0.75_f32],
        );
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
        let source = SamplesBuffer::new(
            std::num::NonZeroU16::new(2).unwrap(),
            std::num::NonZeroU32::new(48_000).unwrap(),
            vec![0.5_f32, 0.75_f32],
        );
        let router = ChannelRouter::new(source, routes, output_settings, 2);
        let output: Vec<f32> = router.take(2).collect();

        assert_eq!(output, vec![0.0, 0.75]);
    }
}
