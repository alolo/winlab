pub mod device_registry;
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OutputChannelInfo {
    pub index: usize,
    pub name: String,
    pub group: String,
    pub active: bool,
}

pub mod engine;
pub mod waveform;

#[cfg(target_os = "windows")]
pub(crate) mod asio_probe;
