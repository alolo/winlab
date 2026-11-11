//! Central registry for output endpoints exposed by each audio backend.
//!
//! CPAL device IDs are persisted as stable endpoint identities. DirectSound
//! currently uses a normalized SDL/Windows endpoint name because SDL2 does not
//! expose the same stable native identifier through the API used by this app.

use crate::config::AudioBackend;
use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError, TrySendError};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceAvailability {
    Available,
    Unavailable,
}

impl DeviceAvailability {
    pub fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioEndpoint {
    /// Stable backend-provided identifier. This is the identity to persist;
    /// `device_name` is presentation text and may change after a driver update.
    pub id: String,
    pub backend: AudioBackend,
    pub device_name: String,
    pub channels: usize,
    pub channel_layouts: Vec<usize>,
    pub availability: DeviceAvailability,
}

impl AudioEndpoint {
    pub fn label(&self) -> String {
        let availability = if self.availability.is_available() {
            ""
        } else {
            " (Unavailable)"
        };
        format!(
            "{} — {}{}",
            self.backend.short_label(),
            self.device_name,
            availability
        )
    }

    pub fn is_available(&self) -> bool {
        self.availability.is_available()
    }
}

#[derive(Debug, Clone, Default)]
pub struct DeviceScanResult {
    pub endpoints: Vec<AudioEndpoint>,
    /// A backend is included only when its enumeration completed successfully.
    /// Failed scans do not mark previously-known endpoints offline.
    pub completed_backends: Vec<AudioBackend>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RegistryChangeSummary {
    pub added: usize,
    pub reconnected: usize,
    pub disconnected: usize,
    pub renamed: usize,
    pub metadata_updated: usize,
}

impl RegistryChangeSummary {
    pub fn changed(self) -> bool {
        self.added + self.reconnected + self.disconnected + self.renamed + self.metadata_updated > 0
    }
}

#[derive(Debug, Default, Clone)]
pub struct DeviceRegistry {
    endpoints: Vec<AudioEndpoint>,
    generation: u64,
    last_scan_at: Option<Instant>,
    last_errors: Vec<String>,
}

impl DeviceRegistry {
    pub fn from_scan(scan: DeviceScanResult) -> Self {
        let mut registry = Self::default();
        registry.apply_scan(scan);
        registry
    }

    pub fn endpoints(&self) -> &[AudioEndpoint] {
        &self.endpoints
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn last_scan_elapsed(&self) -> Option<Duration> {
        self.last_scan_at.as_ref().map(|instant| instant.elapsed())
    }

    pub fn last_errors(&self) -> &[String] {
        &self.last_errors
    }

    pub fn available_count(&self) -> usize {
        self.endpoints
            .iter()
            .filter(|endpoint| endpoint.is_available())
            .count()
    }

    pub fn unavailable_count(&self) -> usize {
        self.endpoints
            .iter()
            .filter(|endpoint| !endpoint.is_available())
            .count()
    }

    pub fn find_by_id(&self, id: &str) -> Option<&AudioEndpoint> {
        self.endpoints.iter().find(|endpoint| endpoint.id == id)
    }

    pub fn find_available_by_id(&self, id: &str) -> Option<&AudioEndpoint> {
        self.find_by_id(id)
            .filter(|endpoint| endpoint.is_available())
    }

    pub fn find_by_backend_name(
        &self,
        backend: AudioBackend,
        device_name: &str,
    ) -> Option<&AudioEndpoint> {
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.backend == backend && endpoint.device_name == device_name)
    }

    pub fn find_available_by_backend_name(
        &self,
        backend: AudioBackend,
        device_name: &str,
    ) -> Option<&AudioEndpoint> {
        self.find_by_backend_name(backend, device_name)
            .filter(|endpoint| endpoint.is_available())
    }

    pub fn id_for(&self, backend: AudioBackend, device_name: &str) -> Option<String> {
        self.find_available_by_backend_name(backend, device_name)
            .map(|endpoint| endpoint.id.clone())
    }

    /// Merge a fresh scan into the registry. Missing devices are retained as
    /// offline records so saved Cue routes can be shown and restored on reconnect.
    pub fn apply_scan(&mut self, scan: DeviceScanResult) -> RegistryChangeSummary {
        let mut changes = RegistryChangeSummary::default();

        // Only mark an endpoint offline when its backend scan completed and
        // that exact stable ID is absent from the new snapshot. Do not first
        // mark every endpoint offline: doing so makes every unchanged device
        // look like a disconnect/reconnect on every five-second hot-plug scan.
        let incoming_ids: std::collections::HashSet<String> = scan
            .endpoints
            .iter()
            .map(|endpoint| endpoint.id.clone())
            .collect();

        for endpoint in &mut self.endpoints {
            if scan.completed_backends.contains(&endpoint.backend)
                && !incoming_ids.contains(&endpoint.id)
                && endpoint.availability.is_available()
            {
                endpoint.availability = DeviceAvailability::Unavailable;
                changes.disconnected += 1;
                warn!(
                    "Audio endpoint disconnected: {} — {} [{}]",
                    endpoint.backend.short_label(),
                    endpoint.device_name,
                    endpoint.id
                );
            }
        }

        for mut incoming in scan.endpoints {
            incoming.availability = DeviceAvailability::Available;
            if let Some(existing) = self
                .endpoints
                .iter_mut()
                .find(|item| item.id == incoming.id)
            {
                if !existing.availability.is_available() {
                    changes.reconnected += 1;
                    info!(
                        "Audio endpoint reconnected: {} — {} [{}]",
                        incoming.backend.short_label(),
                        incoming.device_name,
                        incoming.id
                    );
                }
                if existing.device_name != incoming.device_name {
                    changes.renamed += 1;
                    info!(
                        "Audio endpoint renamed: '{}' -> '{}' [{}]",
                        existing.device_name, incoming.device_name, incoming.id
                    );
                }
                if (!incoming.channel_layouts.is_empty()
                    && existing.channel_layouts != incoming.channel_layouts)
                    || (incoming.channels > 0 && existing.channels != incoming.channels)
                {
                    changes.metadata_updated += 1;
                }

                existing.backend = incoming.backend;
                existing.device_name = incoming.device_name;
                if incoming.channels > 0 {
                    existing.channels = incoming.channels;
                }
                if !incoming.channel_layouts.is_empty() {
                    existing.channel_layouts = incoming.channel_layouts;
                }
                existing.availability = DeviceAvailability::Available;
            } else {
                changes.added += 1;
                info!(
                    "Audio endpoint discovered: {} — {} [{}], channels={}, layouts={:?}",
                    incoming.backend.short_label(),
                    incoming.device_name,
                    incoming.id,
                    incoming.channels,
                    incoming.channel_layouts
                );
                self.endpoints.push(incoming);
            }
        }

        self.endpoints.sort_by(|left, right| {
            left.backend
                .short_label()
                .cmp(right.backend.short_label())
                .then_with(|| right.is_available().cmp(&left.is_available()))
                .then_with(|| left.device_name.cmp(&right.device_name))
                .then_with(|| left.id.cmp(&right.id))
        });

        self.generation = self.generation.wrapping_add(1);
        self.last_scan_at = Some(Instant::now());
        self.last_errors = scan.errors;
        for error in &self.last_errors {
            warn!("Audio device scan: {}", error);
        }

        if changes.changed() {
            info!(
                "Device registry updated: generation={}, added={}, reconnected={}, disconnected={}, renamed={}, metadata_updated={}, available={}, unavailable={}",
                self.generation,
                changes.added,
                changes.reconnected,
                changes.disconnected,
                changes.renamed,
                changes.metadata_updated,
                self.available_count(),
                self.unavailable_count()
            );
        } else {
            debug!(
                "Device registry scan completed: generation={}, available={}, unavailable={}",
                self.generation,
                self.available_count(),
                self.unavailable_count()
            );
        }

        changes
    }

    /// Add or refresh a device discovered by the active audio engine. This is
    /// used to enrich the lightweight background snapshot with real channel data.
    pub fn upsert_available(&mut self, mut endpoint: AudioEndpoint) {
        endpoint.availability = DeviceAvailability::Available;
        if let Some(existing) = self
            .endpoints
            .iter_mut()
            .find(|item| item.id == endpoint.id)
        {
            existing.backend = endpoint.backend;
            existing.device_name = endpoint.device_name;
            if endpoint.channels > 0 {
                existing.channels = endpoint.channels;
            }
            if !endpoint.channel_layouts.is_empty() {
                existing.channel_layouts = endpoint.channel_layouts;
            }
            existing.availability = DeviceAvailability::Available;
        } else {
            self.endpoints.push(endpoint);
        }
        self.endpoints.sort_by(|left, right| {
            left.backend
                .short_label()
                .cmp(right.backend.short_label())
                .then_with(|| right.is_available().cmp(&left.is_available()))
                .then_with(|| left.device_name.cmp(&right.device_name))
                .then_with(|| left.id.cmp(&right.id))
        });
    }
}

/// Background polling keeps device enumeration off the egui/UI thread. A manual
/// refresh sends an immediate request; otherwise the registry is rescanned at
/// the configured interval.
pub struct DeviceRegistryMonitor {
    request_tx: Sender<()>,
    result_rx: Receiver<DeviceScanResult>,
}

impl DeviceRegistryMonitor {
    pub fn spawn(scanner: fn() -> DeviceScanResult, interval: Duration) -> Self {
        let (request_tx, request_rx) = bounded::<()>(1);
        let (result_tx, result_rx) = bounded::<DeviceScanResult>(1);

        thread::Builder::new()
            .name("audio-device-registry".to_string())
            .spawn(move || loop {
                match request_rx.recv_timeout(interval) {
                    Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        let result = scanner();
                        match result_tx.try_send(result) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => {
                                debug!("Skipping device scan result because the UI has not consumed the previous snapshot");
                            }
                            Err(TrySendError::Disconnected(_)) => break,
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
            })
            .expect("Could not start audio device registry monitor thread");

        let monitor = Self {
            request_tx,
            result_rx,
        };
        monitor.request_refresh();
        monitor
    }

    /// Refresh endpoints that are safe to poll while an ASIO stream may be active.
    pub fn request_refresh(&self) {
        let _ = self.request_tx.try_send(());
    }

    pub fn try_recv(&self) -> Option<DeviceScanResult> {
        match self.result_rx.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioEndpoint, DeviceAvailability, DeviceRegistry, DeviceScanResult};
    use crate::config::AudioBackend;

    fn endpoint(
        id: &str,
        backend: AudioBackend,
        name: &str,
        channels: usize,
        layouts: Vec<usize>,
    ) -> AudioEndpoint {
        AudioEndpoint {
            id: id.to_string(),
            backend,
            device_name: name.to_string(),
            channels,
            channel_layouts: layouts,
            availability: DeviceAvailability::Available,
        }
    }

    #[test]
    fn endpoint_label_shows_backend_and_offline_state() {
        let mut endpoint = endpoint(
            "asio:{FOO}",
            AudioBackend::Asio,
            "Focusrite USB ASIO",
            8,
            vec![2, 4, 8],
        );
        assert_eq!(endpoint.label(), "ASIO — Focusrite USB ASIO");
        endpoint.availability = DeviceAvailability::Unavailable;
        assert_eq!(endpoint.label(), "ASIO — Focusrite USB ASIO (Unavailable)");
    }

    #[test]
    fn stable_id_survives_display_name_change() {
        let mut registry = DeviceRegistry::from_scan(DeviceScanResult {
            endpoints: vec![endpoint(
                "wasapi:{A1B2}",
                AudioBackend::Wasapi,
                "Speakers (USB Audio)",
                2,
                vec![2],
            )],
            completed_backends: vec![AudioBackend::Wasapi],
            errors: Vec::new(),
        });

        let changes = registry.apply_scan(DeviceScanResult {
            endpoints: vec![endpoint(
                "wasapi:{A1B2}",
                AudioBackend::Wasapi,
                "Speakers (USB Audio 2)",
                8,
                vec![2, 6, 8],
            )],
            completed_backends: vec![AudioBackend::Wasapi],
            errors: Vec::new(),
        });

        let found = registry.find_available_by_id("wasapi:{A1B2}").unwrap();
        assert_eq!(found.device_name, "Speakers (USB Audio 2)");
        assert_eq!(found.channels, 8);
        assert_eq!(found.channel_layouts, vec![2, 6, 8]);
        assert_eq!(changes.renamed, 1);
        assert_eq!(changes.metadata_updated, 1);
    }

    #[test]
    fn identical_successful_scan_does_not_report_disconnect_and_reconnect() {
        let mut registry = DeviceRegistry::from_scan(DeviceScanResult {
            endpoints: vec![endpoint(
                "wasapi:{A1B2}",
                AudioBackend::Wasapi,
                "Speakers",
                2,
                vec![2],
            )],
            completed_backends: vec![AudioBackend::Wasapi],
            errors: Vec::new(),
        });

        let changes = registry.apply_scan(DeviceScanResult {
            endpoints: vec![endpoint(
                "wasapi:{A1B2}",
                AudioBackend::Wasapi,
                "Speakers",
                2,
                vec![2],
            )],
            completed_backends: vec![AudioBackend::Wasapi],
            errors: Vec::new(),
        });

        assert!(!changes.changed());
        assert_eq!(changes.disconnected, 0);
        assert_eq!(changes.reconnected, 0);
        assert_eq!(registry.available_count(), 1);
        assert_eq!(registry.unavailable_count(), 0);
    }

    #[test]
    fn disconnected_endpoint_is_retained_and_reconnected_by_id() {
        let mut registry = DeviceRegistry::from_scan(DeviceScanResult {
            endpoints: vec![endpoint(
                "asio:{FOO}",
                AudioBackend::Asio,
                "Focusrite USB ASIO",
                8,
                vec![2, 4, 8],
            )],
            completed_backends: vec![AudioBackend::Asio],
            errors: Vec::new(),
        });

        let disconnected = registry.apply_scan(DeviceScanResult {
            endpoints: Vec::new(),
            completed_backends: vec![AudioBackend::Asio],
            errors: Vec::new(),
        });
        assert_eq!(disconnected.disconnected, 1);
        assert_eq!(registry.unavailable_count(), 1);
        assert!(registry.find_by_id("asio:{FOO}").is_some());
        assert!(registry.find_available_by_id("asio:{FOO}").is_none());

        let reconnected = registry.apply_scan(DeviceScanResult {
            endpoints: vec![endpoint(
                "asio:{FOO}",
                AudioBackend::Asio,
                "Focusrite USB ASIO",
                8,
                vec![2, 4, 8],
            )],
            completed_backends: vec![AudioBackend::Asio],
            errors: Vec::new(),
        });
        assert_eq!(reconnected.reconnected, 1);
        assert_eq!(registry.available_count(), 1);
        assert!(registry.find_available_by_id("asio:{FOO}").is_some());
    }

    #[test]
    fn failed_backend_scan_does_not_mark_devices_offline() {
        let mut registry = DeviceRegistry::from_scan(DeviceScanResult {
            endpoints: vec![endpoint(
                "wasapi:{A1B2}",
                AudioBackend::Wasapi,
                "Speakers",
                2,
                vec![2],
            )],
            completed_backends: vec![AudioBackend::Wasapi],
            errors: Vec::new(),
        });

        registry.apply_scan(DeviceScanResult {
            endpoints: Vec::new(),
            completed_backends: Vec::new(),
            errors: vec!["WASAPI enumeration failed".to_string()],
        });

        assert!(registry.find_available_by_id("wasapi:{A1B2}").is_some());
        assert_eq!(registry.unavailable_count(), 0);
        assert_eq!(registry.last_errors().len(), 1);
    }

    #[test]
    fn lightweight_scan_keeps_previously_probed_channel_metadata() {
        let mut registry = DeviceRegistry::from_scan(DeviceScanResult {
            endpoints: vec![endpoint(
                "wasapi:{A1B2}",
                AudioBackend::Wasapi,
                "Speakers",
                8,
                vec![2, 6, 8],
            )],
            completed_backends: vec![AudioBackend::Wasapi],
            errors: Vec::new(),
        });

        registry.apply_scan(DeviceScanResult {
            endpoints: vec![endpoint(
                "wasapi:{A1B2}",
                AudioBackend::Wasapi,
                "Speakers",
                0,
                Vec::new(),
            )],
            completed_backends: vec![AudioBackend::Wasapi],
            errors: Vec::new(),
        });

        let found = registry.find_available_by_id("wasapi:{A1B2}").unwrap();
        assert_eq!(found.channels, 8);
        assert_eq!(found.channel_layouts, vec![2, 6, 8]);
    }
}
