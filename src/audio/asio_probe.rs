//! Isolated ASIO metadata probe.
//!
//! CPAL exposes ASIO stream channel counts, but not the driver's per-channel
//! names. We query those names in a short-lived child process so a broken or
//! non-responsive native driver cannot freeze the Theatre Show Cue UI process.

use super::OutputChannelInfo;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AsioDriverProbeResult {
    driver_name: String,
    output_channels: Vec<OutputChannelInfo>,
}

/// Query every registered ASIO driver in a separate process.
///
/// A driver is allowed to hang during COM initialization. Keeping that work in
/// a child process lets the main application terminate the probe and continue
/// with generic channel labels instead of becoming unresponsive.
pub(crate) fn probe_registered_drivers() -> HashMap<String, Vec<OutputChannelInfo>> {
    use azo::driver::Metadata;

    let metadata = match Metadata::enumerate() {
        Ok(metadata) => metadata,
        Err(error) => {
            warn!("Could not enumerate ASIO driver metadata: {error}");
            return HashMap::new();
        }
    };

    if metadata.is_empty() {
        info!("No registered ASIO drivers found for channel-name discovery");
        return HashMap::new();
    }

    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            warn!("Could not locate TaSQ executable for ASIO probe: {error}");
            return HashMap::new();
        }
    };

    let mut discovered = HashMap::new();
    let mut failed_probes = 0usize;
    let total_started = Instant::now();
    let total_timeout = Duration::from_secs(12);
    for driver_index in 0..metadata.len() {
        if total_started.elapsed() >= total_timeout {
            warn!("ASIO metadata discovery reached its 12-second time budget; remaining drivers will use generic labels");
            break;
        }

        let output_path =
            std::env::temp_dir().join(format!("tasq-asio-probe-{}.json", Uuid::new_v4()));

        let mut child = match Command::new(&executable)
            .arg("--tasq-asio-probe")
            .arg(driver_index.to_string())
            .arg(&output_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                warn!("Could not start ASIO metadata probe #{driver_index}: {error}");
                continue;
            }
        };

        let started = Instant::now();
        let timeout = Duration::from_secs(3);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if started.elapsed() < timeout => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(None) => {
                    warn!(
                        "ASIO metadata probe #{driver_index} timed out; terminating probe process"
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                Err(error) => {
                    warn!("Could not wait for ASIO metadata probe #{driver_index}: {error}");
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };

        let probe_succeeded = status.as_ref().is_some_and(|status| status.success());
        let probe_exited = status.is_some();
        if probe_succeeded {
            match std::fs::read(&output_path)
                .context("read ASIO probe output")
                .and_then(|bytes| {
                    serde_json::from_slice::<AsioDriverProbeResult>(&bytes)
                        .context("parse ASIO probe output")
                }) {
                Ok(result) if !result.output_channels.is_empty() => {
                    info!(
                        "ASIO driver '{}' reports {} output channels: {:?}",
                        result.driver_name,
                        result.output_channels.len(),
                        result
                            .output_channels
                            .iter()
                            .map(|channel| (
                                &channel.name,
                                channel.index,
                                channel.group.clone(),
                                channel.active
                            ))
                            .collect::<Vec<_>>()
                    );
                    discovered.insert(result.driver_name, result.output_channels);
                }
                Ok(result) => {
                    warn!(
                        "ASIO driver '{}' did not report output channels",
                        result.driver_name
                    );
                }
                Err(error) => {
                    warn!("Could not read ASIO metadata probe #{driver_index}: {error:#}");
                }
            }
        } else if probe_exited {
            failed_probes += 1;
            let diagnostic_path = output_path.with_extension("error.txt");
            let detail = std::fs::read_to_string(&diagnostic_path)
                .unwrap_or_else(|_| "the helper exited without writing diagnostics".to_string());
            debug!(
                "ASIO metadata probe #{} failed (non-fatal; generic channel names will be used): {}",
                driver_index,
                detail.trim()
            );
            let _ = std::fs::remove_file(diagnostic_path);
        }

        let _ = std::fs::remove_file(&output_path);
    }

    if failed_probes > 0 {
        info!(
            "ASIO metadata discovery completed: {} driver(s) described, {} probe(s) failed; playback can continue with generic channel labels",
            discovered.len(),
            failed_probes
        );
    }
    discovered
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
