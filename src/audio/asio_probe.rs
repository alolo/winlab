//! Isolated ASIO metadata probe.
//!
//! CPAL exposes ASIO stream channel counts, but not the driver's per-channel
//! names. We query those names in a short-lived child process so a broken or
//! non-responsive native driver cannot freeze the Theatre Show Cue UI process.

use super::OutputChannelInfo;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AsioDriverProbeResult {
    driver_name: String,
    output_channels: Vec<OutputChannelInfo>,
}

/// Entry point used only by the short-lived `--tasq-asio-probe` child.
pub(crate) fn run_probe_child(driver_index: usize, output_path: &Path) -> Result<()> {
    use azo::driver::{Driver, Metadata, SafeHandle};
    use azo::dto::ChannelId;

    let metadata = Metadata::enumerate().context("enumerate ASIO drivers in probe child")?;
    let driver_metadata = metadata
        .get(driver_index)
        .ok_or_else(|| anyhow::anyhow!("ASIO driver index {driver_index} no longer exists"))?;

    let driver = SafeHandle::new(&driver_metadata.clsid).context("create ASIO driver instance")?;
    if !driver.init(None) {
        anyhow::bail!(
            "ASIO driver initialization failed: {}",
            driver.last_error().to_string_lossy()
        );
    }

    let driver_name = driver.name().to_string_lossy().into_owned();
    let counts = driver
        .channel_counts()
        .context("query ASIO channel counts")?;
    let mut output_channels = Vec::with_capacity(counts.out.max(0) as usize);

    for channel_index in 0..counts.out {
        let channel = driver
            .channel_info(ChannelId {
                input: false,
                index: channel_index,
            })
            .with_context(|| format!("query ASIO output channel {channel_index}"))?;

        output_channels.push(OutputChannelInfo {
            index: channel_index as usize,
            name: channel.name.to_string_lossy().into_owned(),
            group: format!("{:?}", channel.group),
            active: channel.is_active,
        });
    }

    // Release the native driver before returning control to the parent process,
    // which will subsequently initialize CPAL's ASIO stream.
    drop(driver);

    let result = AsioDriverProbeResult {
        driver_name,
        output_channels,
    };
    let bytes = serde_json::to_vec(&result).context("serialize ASIO metadata")?;
    std::fs::write(output_path, bytes).context("write ASIO metadata probe result")?;
    Ok(())
}
