//! Central registry for output endpoints exposed by each audio backend.
//!
//! CPAL device IDs are persisted as stable endpoint identities. DirectSound
//! currently uses a normalized SDL/Windows endpoint name because SDL2 does not
//! expose the same stable native identifier through the API used by this app.

use crate::config::AudioBackend;
use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError, TrySendError};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Windows Core Audio endpoint notifications. The COM callback only enqueues a
/// lightweight refresh request; enumeration always runs on the registry thread,
/// never on the COM callback or audio-render thread.
#[cfg(target_os = "windows")]
mod windows_endpoint_notifications {
    use crossbeam_channel::{Receiver, Sender, TrySendError};
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use std::sync::atomic::{fence, AtomicU32, Ordering};
    use tracing::{debug, info, warn};

    type HResult = i32;

    #[repr(C)]
    #[derive(Clone, Copy, PartialEq, Eq)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    const CLSID_MM_DEVICE_ENUMERATOR: Guid = Guid {
        data1: 0xBCDE0395,
        data2: 0xE52F,
        data3: 0x467C,
        data4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
    };
    const IID_IMM_DEVICE_ENUMERATOR: Guid = Guid {
        data1: 0xA95664D2,
        data2: 0x9614,
        data3: 0x4F35,
        data4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
    };
    const IID_IMM_NOTIFICATION_CLIENT: Guid = Guid {
        data1: 0x7991EEC9,
        data2: 0x7E89,
        data3: 0x4D85,
        data4: [0x83, 0x90, 0x6C, 0x70, 0x3C, 0xEC, 0x60, 0xC0],
    };
    const IID_IUNKNOWN: Guid = Guid {
        data1: 0x00000000,
        data2: 0x0000,
        data3: 0x0000,
        data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
    };

    const S_OK: HResult = 0;
    const E_POINTER: HResult = 0x8000_4003_u32 as i32;
    const E_NOINTERFACE: HResult = 0x8000_4002_u32 as i32;
    const CLSCTX_INPROC_SERVER: u32 = 0x1;
    const COINIT_MULTITHREADED: u32 = 0x0;

    type QueryInterfaceFn =
        unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> HResult;
    type AddRefFn = unsafe extern "system" fn(*mut c_void) -> u32;
    type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;

    #[repr(C)]
    struct NotificationClientVTable {
        query_interface: QueryInterfaceFn,
        add_ref: AddRefFn,
        release: ReleaseFn,
        on_device_state_changed: unsafe extern "system" fn(*mut c_void, *const u16, u32) -> HResult,
        on_device_added: unsafe extern "system" fn(*mut c_void, *const u16) -> HResult,
        on_device_removed: unsafe extern "system" fn(*mut c_void, *const u16) -> HResult,
        on_default_device_changed:
            unsafe extern "system" fn(*mut c_void, i32, i32, *const u16) -> HResult,
        on_property_value_changed:
            unsafe extern "system" fn(*mut c_void, *const u16, *const c_void) -> HResult,
    }

    #[repr(C)]
    struct NotificationClient {
        #[allow(dead_code)]
        vtable: *const NotificationClientVTable,
        references: AtomicU32,
        refresh_tx: Sender<()>,
    }

    // The vtable is static and immutable; Sender is thread-safe. Core Audio may
    // invoke the COM callbacks from an RPC thread different from the registrar.
    unsafe impl Send for NotificationClient {}
    unsafe impl Sync for NotificationClient {}

    unsafe extern "system" fn query_interface(
        this: *mut c_void,
        interface_id: *const Guid,
        output: *mut *mut c_void,
    ) -> HResult {
        if this.is_null() || interface_id.is_null() || output.is_null() {
            return E_POINTER;
        }
        *output = std::ptr::null_mut();
        if *interface_id == IID_IUNKNOWN || *interface_id == IID_IMM_NOTIFICATION_CLIENT {
            *output = this;
            add_ref(this);
            S_OK
        } else {
            E_NOINTERFACE
        }
    }

    unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
        if this.is_null() {
            return 0;
        }
        let client = &*(this as *mut NotificationClient);
        client.references.fetch_add(1, Ordering::Relaxed) + 1
    }

    unsafe extern "system" fn release(this: *mut c_void) -> u32 {
        if this.is_null() {
            return 0;
        }
        let client = &*(this as *mut NotificationClient);
        let remaining = client.references.fetch_sub(1, Ordering::Release) - 1;
        if remaining == 0 {
            fence(Ordering::Acquire);
            drop(Box::from_raw(this as *mut NotificationClient));
        }
        remaining
    }

    fn request_refresh(this: *mut c_void, event_name: &'static str) -> HResult {
        if !this.is_null() {
            // A bounded channel coalesces bursts such as device-state + default
            // endpoint notifications into one background enumeration.
            let client = unsafe { &*(this as *mut NotificationClient) };
            match client.refresh_tx.try_send(()) {
                Ok(()) => debug!(
                    "Windows Core Audio notification received: {}; refresh queued",
                    event_name
                ),
                Err(TrySendError::Full(_)) => debug!(
                    "Windows Core Audio notification received: {}; refresh already queued",
                    event_name
                ),
                Err(TrySendError::Disconnected(_)) => warn!(
                    "Windows Core Audio notification received: {}; registry monitor is unavailable",
                    event_name
                ),
            }
        }
        S_OK
    }

    unsafe extern "system" fn on_device_state_changed(
        this: *mut c_void,
        _device_id: *const u16,
        _new_state: u32,
    ) -> HResult {
        request_refresh(this, "OnDeviceStateChanged")
    }

    unsafe extern "system" fn on_device_added(
        this: *mut c_void,
        _device_id: *const u16,
    ) -> HResult {
        request_refresh(this, "OnDeviceAdded")
    }

    unsafe extern "system" fn on_device_removed(
        this: *mut c_void,
        _device_id: *const u16,
    ) -> HResult {
        request_refresh(this, "OnDeviceRemoved")
    }

    unsafe extern "system" fn on_default_device_changed(
        this: *mut c_void,
        _flow: i32,
        _role: i32,
        _default_device_id: *const u16,
    ) -> HResult {
        request_refresh(this, "OnDefaultDeviceChanged")
    }

    unsafe extern "system" fn on_property_value_changed(
        _this: *mut c_void,
        _device_id: *const u16,
        _property_key: *const c_void,
    ) -> HResult {
        // Endpoint property notifications can also represent volume/mute
        // changes. They do not affect the endpoint inventory, so avoid scans.
        S_OK
    }

    static NOTIFICATION_CLIENT_VTABLE: NotificationClientVTable = NotificationClientVTable {
        query_interface,
        add_ref,
        release,
        on_device_state_changed,
        on_device_added,
        on_device_removed,
        on_default_device_changed,
        on_property_value_changed,
    };

    #[repr(C)]
    #[allow(dead_code)]
    struct DeviceEnumeratorVTable {
        query_interface: QueryInterfaceFn,
        add_ref: AddRefFn,
        release: ReleaseFn,
        enum_audio_endpoints: *const c_void,
        get_default_audio_endpoint: *const c_void,
        get_device: *const c_void,
        register_endpoint_notification_callback:
            unsafe extern "system" fn(*mut c_void, *mut c_void) -> HResult,
        unregister_endpoint_notification_callback:
            unsafe extern "system" fn(*mut c_void, *mut c_void) -> HResult,
    }

    #[link(name = "ole32")]
    extern "system" {
        fn CoInitializeEx(reserved: *mut c_void, coinit: u32) -> HResult;
        fn CoUninitialize();
        fn CoCreateInstance(
            class_id: *const Guid,
            outer: *mut c_void,
            context: u32,
            interface_id: *const Guid,
            object: *mut *mut c_void,
        ) -> HResult;
    }

    pub fn run(refresh_tx: Sender<()>, stop_rx: Receiver<()>) {
        let initialized = unsafe { CoInitializeEx(null_mut(), COINIT_MULTITHREADED) };
        if initialized < 0 {
            warn!(
                "Could not initialize the Core Audio notification COM apartment (HRESULT=0x{:08X}); 60-second fallback scanning remains enabled",
                initialized as u32
            );
            return;
        }

        let mut enumerator = null_mut();
        let created = unsafe {
            CoCreateInstance(
                &CLSID_MM_DEVICE_ENUMERATOR,
                null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_IMM_DEVICE_ENUMERATOR,
                &mut enumerator,
            )
        };
        if created < 0 || enumerator.is_null() {
            warn!(
                "Could not create IMMDeviceEnumerator for hot-plug notifications (HRESULT=0x{:08X}); 60-second fallback scanning remains enabled",
                created as u32
            );
            unsafe { CoUninitialize() };
            return;
        }

        let client = Box::into_raw(Box::new(NotificationClient {
            vtable: &NOTIFICATION_CLIENT_VTABLE,
            references: AtomicU32::new(1),
            refresh_tx,
        }));

        let enumerator_vtable = unsafe { *(enumerator as *mut *const DeviceEnumeratorVTable) };
        let registered = unsafe {
            ((*enumerator_vtable).register_endpoint_notification_callback)(
                enumerator,
                client as *mut c_void,
            )
        };

        if registered < 0 {
            warn!(
                "Could not register IMMNotificationClient (HRESULT=0x{:08X}); 60-second fallback scanning remains enabled",
                registered as u32
            );
            unsafe {
                release(client as *mut c_void);
                ((*enumerator_vtable).release)(enumerator);
                CoUninitialize();
            }
            return;
        }

        info!("Windows Core Audio endpoint notifications registered");

        // The COM callback is deliberately tiny. It only queues a refresh;
        // enumeration and registry mutation happen on the separate monitor thread.
        let _ = stop_rx.recv();

        unsafe {
            let unregistered = ((*enumerator_vtable).unregister_endpoint_notification_callback)(
                enumerator,
                client as *mut c_void,
            );
            if unregistered < 0 {
                warn!(
                    "Could not unregister IMMNotificationClient cleanly (HRESULT=0x{:08X})",
                    unregistered as u32
                );
            }
            ((*enumerator_vtable).release)(enumerator);
            // Drop the registrar's reference. Core Audio releases its reference
            // during UnregisterEndpointNotificationCallback.
            release(client as *mut c_void);
            CoUninitialize();
        }

        info!("Windows Core Audio endpoint notifications stopped");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceAvailability {
    Available,
    Unavailable,
}

impl DeviceAvailability {
    pub fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// ASIO host enumeration is intentionally avoided during playback because some
/// vendor drivers are sensitive to repeated host initialization. On Windows,
/// the WDM/WASAPI endpoint for the same physical interface is still monitored.
/// Match only exact model-like tokens containing digits (for example `us366`)
/// so generic vendor words such as `USB` or `Focusrite` cannot cross-disable
/// unrelated devices.
fn hardware_model_tokens(device_name: &str) -> HashSet<String> {
    let mut tokens = HashSet::new();
    let mut current = String::new();

    for character in device_name.chars().chain(std::iter::once(' ')) {
        if character.is_ascii_alphanumeric() {
            current.push(character.to_ascii_lowercase());
        } else if matches!(character, '-' | '_') && !current.is_empty() {
            // Preserve separators commonly used inside hardware model names:
            // `US-366` and `US_366` both normalize to `us366`.
            continue;
        } else if !current.is_empty() {
            let has_letter = current.chars().any(|value| value.is_ascii_alphabetic());
            let has_digit = current.chars().any(|value| value.is_ascii_digit());
            if current.len() >= 3 && has_letter && has_digit {
                tokens.insert(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
    }

    tokens
}

fn shares_hardware_model_token(left: &str, right: &str) -> bool {
    let left_tokens = hardware_model_tokens(left);
    let right_tokens = hardware_model_tokens(right);
    left_tokens.iter().any(|token| right_tokens.contains(token))
}

fn unique_matching_asio_endpoint_id(
    endpoints: &[AudioEndpoint],
    windows_device_name: &str,
    expected_availability: DeviceAvailability,
) -> Option<String> {
    let matches: Vec<&AudioEndpoint> = endpoints
        .iter()
        .filter(|candidate| {
            candidate.backend == AudioBackend::Asio
                && candidate.availability == expected_availability
                && shares_hardware_model_token(&candidate.device_name, windows_device_name)
        })
        .collect();

    match matches.as_slice() {
        [endpoint] => Some(endpoint.id.clone()),
        [] => None,
        _ => {
            // Identical model tokens are ambiguous when multiple ASIO devices
            // are present. Do not risk disabling/restarting the wrong device.
            warn!(
                "ASIO hot-plug correlation skipped: Windows endpoint '{}' matches {} ASIO endpoints by model token",
                windows_device_name,
                matches.len()
            );
            None
        }
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

    /// Rebuild a registry snapshot received from an isolated audio worker.
    /// The scan timestamp and generation are process-local and intentionally
    /// restart from their defaults; endpoint identities and availability remain.
    pub fn from_endpoints(endpoints: Vec<AudioEndpoint>) -> Self {
        let generation = if endpoints.is_empty() { 0 } else { 1 };
        Self {
            endpoints,
            generation,
            ..Self::default()
        }
    }

    /// Copy the endpoint records for transfer across the worker IPC boundary.
    pub fn snapshot_endpoints(&self) -> Vec<AudioEndpoint> {
        self.endpoints.clone()
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

        let mut newly_disconnected = Vec::new();
        for endpoint in &mut self.endpoints {
            if scan.completed_backends.contains(&endpoint.backend)
                && !incoming_ids.contains(&endpoint.id)
                && endpoint.availability.is_available()
            {
                endpoint.availability = DeviceAvailability::Unavailable;
                changes.disconnected += 1;
                newly_disconnected.push(endpoint.clone());
                warn!(
                    "Audio endpoint disconnected: {} — {} [{}]",
                    endpoint.backend.short_label(),
                    endpoint.device_name,
                    endpoint.id
                );
            }
        }

        // ASIO is not included in periodic hot-plug enumeration. When Windows
        // reports that a WDM endpoint disappeared, propagate that physical
        // device state to a uniquely model-matched ASIO endpoint. This lets the
        // worker manager stop affected Cues and recover the ASIO process without
        // repeatedly reinitializing the ASIO host while its stream is active.
        for disconnected in newly_disconnected
            .iter()
            .filter(|endpoint| endpoint.backend != AudioBackend::Asio)
        {
            let related_asio_id = unique_matching_asio_endpoint_id(
                &self.endpoints,
                &disconnected.device_name,
                DeviceAvailability::Available,
            );

            if let Some(related_asio_id) = related_asio_id {
                if let Some(endpoint) = self.endpoints.iter_mut().find(|endpoint| {
                    endpoint.backend == AudioBackend::Asio
                        && endpoint.id == related_asio_id
                        && endpoint.availability.is_available()
                }) {
                    endpoint.availability = DeviceAvailability::Unavailable;
                    changes.disconnected += 1;
                    warn!(
                        "Related ASIO endpoint marked unavailable because its Windows audio endpoint disappeared: ASIO — '{}' [{}], related endpoint='{}' ({})",
                        endpoint.device_name,
                        endpoint.id,
                        disconnected.device_name,
                        disconnected.backend.short_label()
                    );
                }
            }
        }

        let mut newly_reconnected = Vec::new();
        for mut incoming in scan.endpoints {
            incoming.availability = DeviceAvailability::Available;
            if let Some(existing) = self
                .endpoints
                .iter_mut()
                .find(|item| item.id == incoming.id)
            {
                if !existing.availability.is_available() {
                    changes.reconnected += 1;
                    newly_reconnected.push(incoming.clone());
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

        // Mirror a returning WDM endpoint to the matching ASIO record. The
        // worker manager receives this registry transition and reopens the
        // ASIO endpoint asynchronously; interrupted Cues remain stopped.
        for reconnected in newly_reconnected
            .iter()
            .filter(|endpoint| endpoint.backend != AudioBackend::Asio)
        {
            let related_asio_id = unique_matching_asio_endpoint_id(
                &self.endpoints,
                &reconnected.device_name,
                DeviceAvailability::Unavailable,
            );

            if let Some(related_asio_id) = related_asio_id {
                if let Some(endpoint) = self.endpoints.iter_mut().find(|endpoint| {
                    endpoint.backend == AudioBackend::Asio
                        && endpoint.id == related_asio_id
                        && !endpoint.availability.is_available()
                }) {
                    endpoint.availability = DeviceAvailability::Available;
                    changes.reconnected += 1;
                    info!(
                        "Related ASIO endpoint marked available because its Windows audio endpoint returned: ASIO — '{}' [{}], related endpoint='{}' ({})",
                        endpoint.device_name,
                        endpoint.id,
                        reconnected.device_name,
                        reconnected.backend.short_label()
                    );
                }
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

/// A Windows endpoint that has been seen at least once by the registry monitor.
/// `present` is updated only when that endpoint's backend scan completed.
#[derive(Debug, Clone)]
struct KnownEndpointPresence {
    backend: AudioBackend,
    id: String,
    present: bool,
}

const RECONNECT_WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// Update the monitor's known Windows endpoint inventory and return the number
/// of previously-known endpoints that are still missing. Failed backend scans
/// leave their previous presence state unchanged.
fn update_known_endpoint_presence(
    known_endpoints: &mut Vec<KnownEndpointPresence>,
    scan: &DeviceScanResult,
) -> usize {
    for backend in &scan.completed_backends {
        for known in known_endpoints
            .iter_mut()
            .filter(|known| known.backend == *backend)
        {
            known.present = scan
                .endpoints
                .iter()
                .any(|endpoint| endpoint.backend == *backend && endpoint.id == known.id);
        }

        for endpoint in scan
            .endpoints
            .iter()
            .filter(|endpoint| endpoint.backend == *backend)
        {
            if !known_endpoints
                .iter()
                .any(|known| known.backend == *backend && known.id == endpoint.id)
            {
                known_endpoints.push(KnownEndpointPresence {
                    backend: *backend,
                    id: endpoint.id.clone(),
                    present: true,
                });
            }
        }
    }

    known_endpoints
        .iter()
        .filter(|known| !known.present)
        .count()
}

/// Publish the newest registry snapshot. If the UI has not consumed an older
/// snapshot yet, discard that stale snapshot and replace it with the latest
/// one instead of losing a reconnect/disconnect transition until the 60s scan.
fn publish_latest_scan_result(
    result_tx: &Sender<DeviceScanResult>,
    stale_result_rx: &Receiver<DeviceScanResult>,
    result: DeviceScanResult,
    source: &str,
) -> bool {
    match result_tx.try_send(result) {
        Ok(()) => true,
        Err(TrySendError::Disconnected(_)) => false,
        Err(TrySendError::Full(mut pending)) => {
            let mut discarded = 0usize;
            while stale_result_rx.try_recv().is_ok() {
                discarded += 1;
            }

            match result_tx.try_send(pending) {
                Ok(()) => {
                    debug!(
                        "Replaced {} stale device registry snapshot(s) with the latest {} snapshot",
                        discarded, source
                    );
                    true
                }
                Err(TrySendError::Full(returned)) => {
                    // This can only occur if the consumer/producer raced the
                    // replacement. Keep the newest snapshot and make one final
                    // best-effort attempt after draining again.
                    pending = returned;
                    while stale_result_rx.try_recv().is_ok() {
                        discarded += 1;
                    }
                    match result_tx.try_send(pending) {
                        Ok(()) => {
                            debug!(
                                "Published latest {} device registry snapshot after replacing {} stale snapshot(s)",
                                source, discarded
                            );
                            true
                        }
                        Err(TrySendError::Full(_)) => {
                            warn!(
                                "Could not publish latest {} device registry snapshot because the result queue remained full",
                                source
                            );
                            true
                        }
                        Err(TrySendError::Disconnected(_)) => false,
                    }
                }
                Err(TrySendError::Disconnected(_)) => false,
            }
        }
    }
}

/// Event-driven Windows endpoint monitoring with a low-frequency fallback.
/// During a known endpoint outage, a lightweight WASAPI/DirectSound reconnect
/// watch runs every two seconds. ASIO host enumeration is never part of this
/// watch. Native enumeration always runs on this background thread, never on
/// the GUI thread, COM notification callback, or audio-render callback.
pub struct DeviceRegistryMonitor {
    request_tx: Sender<()>,
    result_rx: Receiver<DeviceScanResult>,
    stop_tx: Sender<()>,
    notification_stop_tx: Option<Sender<()>>,
}

impl DeviceRegistryMonitor {
    pub fn spawn(
        scanner: fn() -> DeviceScanResult,
        fallback_interval: Duration,
        initial_endpoints: Vec<AudioEndpoint>,
    ) -> Self {
        let (request_tx, request_rx) = bounded::<()>(1);
        let (result_tx, result_rx) = bounded::<DeviceScanResult>(1);
        let stale_result_rx = result_rx.clone();
        let (stop_tx, stop_rx) = bounded::<()>(1);
        let (notification_stop_tx, notification_stop_rx) = bounded::<()>(1);

        #[cfg(target_os = "windows")]
        {
            let notification_refresh_tx = request_tx.clone();
            thread::Builder::new()
                .name("tasq-windows-audio-notifications".to_string())
                .spawn(move || {
                    windows_endpoint_notifications::run(
                        notification_refresh_tx,
                        notification_stop_rx,
                    );
                })
                .expect("Could not start Windows audio notification thread");
        }

        #[cfg(not(target_os = "windows"))]
        let _ = (notification_stop_tx, notification_stop_rx);

        thread::Builder::new()
            .name("audio-device-registry".to_string())
            .spawn(move || {
                let mut known_endpoints: Vec<KnownEndpointPresence> = initial_endpoints
                    .into_iter()
                    .filter(|endpoint| {
                        matches!(
                            endpoint.backend,
                            AudioBackend::Wasapi | AudioBackend::DirectSound
                        )
                    })
                    .map(|endpoint| {
                        let present = endpoint.is_available();
                        KnownEndpointPresence {
                            backend: endpoint.backend,
                            id: endpoint.id,
                            present,
                        }
                    })
                    .collect();
                let mut reconnect_watch_active = known_endpoints.iter().any(|known| !known.present);
                let mut next_fallback = Instant::now()
                    + if reconnect_watch_active {
                        RECONNECT_WATCH_INTERVAL
                    } else {
                        fallback_interval
                    };

                loop {
                    let wait = next_fallback.saturating_duration_since(Instant::now());
                    crossbeam_channel::select! {
                        recv(stop_rx) -> _ => break,
                        recv(request_rx) -> request => {
                            if request.is_err() {
                                break;
                            }
                            // Coalesce bursts of endpoint notifications (for example,
                            // remove + state-change + default-device-change) into one scan.
                            while request_rx.try_recv().is_ok() {}
                            let scan_started = Instant::now();
                            let result = scanner();
                            debug!(
                                "Event-triggered audio device scan completed in {:.2} ms: {} endpoint(s), {} completed backend(s), {} error(s)",
                                scan_started.elapsed().as_secs_f64() * 1_000.0,
                                result.endpoints.len(),
                                result.completed_backends.len(),
                                result.errors.len()
                            );
                            let missing_count =
                                update_known_endpoint_presence(&mut known_endpoints, &result);
                            let next_watch_active = missing_count > 0;
                            if next_watch_active != reconnect_watch_active {
                                if next_watch_active {
                                    info!(
                                        "Audio endpoint reconnect watch enabled: {} known Windows endpoint(s) unavailable; probing WASAPI/DirectSound every {}s (ASIO host enumeration remains disabled)",
                                        missing_count,
                                        RECONNECT_WATCH_INTERVAL.as_secs()
                                    );
                                } else {
                                    info!(
                                        "Audio endpoint reconnect watch disabled: all previously-known Windows endpoints are present; fallback interval restored to {}s",
                                        fallback_interval.as_secs()
                                    );
                                }
                                reconnect_watch_active = next_watch_active;
                            }
                            if !publish_latest_scan_result(
                                &result_tx,
                                &stale_result_rx,
                                result,
                                "event-triggered",
                            ) {
                                break;
                            }
                            next_fallback = Instant::now()
                                + if reconnect_watch_active {
                                    RECONNECT_WATCH_INTERVAL
                                } else {
                                    fallback_interval
                                };
                        }
                        default(wait) => {
                            let scan_started = Instant::now();
                            let result = scanner();
                            debug!(
                                "{} audio device scan completed in {:.2} ms: {} endpoint(s), {} completed backend(s), {} error(s)",
                                if reconnect_watch_active { "Reconnect-watch" } else { "Fallback" },
                                scan_started.elapsed().as_secs_f64() * 1_000.0,
                                result.endpoints.len(),
                                result.completed_backends.len(),
                                result.errors.len()
                            );
                            let missing_count =
                                update_known_endpoint_presence(&mut known_endpoints, &result);
                            let next_watch_active = missing_count > 0;
                            if next_watch_active != reconnect_watch_active {
                                if next_watch_active {
                                    info!(
                                        "Audio endpoint reconnect watch enabled: {} known Windows endpoint(s) unavailable; probing WASAPI/DirectSound every {}s (ASIO host enumeration remains disabled)",
                                        missing_count,
                                        RECONNECT_WATCH_INTERVAL.as_secs()
                                    );
                                } else {
                                    info!(
                                        "Audio endpoint reconnect watch disabled: all previously-known Windows endpoints are present; fallback interval restored to {}s",
                                        fallback_interval.as_secs()
                                    );
                                }
                                reconnect_watch_active = next_watch_active;
                            }
                            if !publish_latest_scan_result(
                                &result_tx,
                                &stale_result_rx,
                                result,
                                if reconnect_watch_active { "reconnect-watch" } else { "fallback" },
                            ) {
                                break;
                            }
                            next_fallback = Instant::now()
                                + if reconnect_watch_active {
                                    RECONNECT_WATCH_INTERVAL
                                } else {
                                    fallback_interval
                                };
                        }
                    }
                }
                debug!("Audio device registry monitor stopped");
            })
            .expect("Could not start audio device registry monitor thread");

        let monitor = Self {
            request_tx,
            result_rx,
            stop_tx,
            #[cfg(target_os = "windows")]
            notification_stop_tx: Some(notification_stop_tx),
            #[cfg(not(target_os = "windows"))]
            notification_stop_tx: None,
        };
        monitor.request_refresh();
        monitor
    }

    /// Request an immediate background refresh. This is used by the manual
    /// Refresh devices button and by Windows Core Audio endpoint notifications.
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

impl Drop for DeviceRegistryMonitor {
    fn drop(&mut self) {
        let _ = self.stop_tx.try_send(());
        if let Some(notification_stop_tx) = self.notification_stop_tx.as_ref() {
            let _ = notification_stop_tx.try_send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        publish_latest_scan_result, shares_hardware_model_token, update_known_endpoint_presence,
        AudioEndpoint, DeviceAvailability, DeviceRegistry, DeviceScanResult, KnownEndpointPresence,
    };
    use crate::config::AudioBackend;
    use crossbeam_channel::bounded;

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

    #[test]
    fn windows_hotplug_marks_matching_asio_endpoint_offline_and_online() {
        let asio = endpoint(
            "asio:US-322 / US-366",
            AudioBackend::Asio,
            "US-322 / US-366",
            6,
            vec![2, 6],
        );
        let wasapi = endpoint(
            "wasapi:{TASCAM}",
            AudioBackend::Wasapi,
            "Динамики (TASCAM US-366)",
            2,
            vec![2],
        );
        let directsound = endpoint(
            "directsound:name:tascamus366",
            AudioBackend::DirectSound,
            "Динамики (TASCAM US-366)",
            2,
            vec![2],
        );
        let mut registry = DeviceRegistry::from_scan(DeviceScanResult {
            endpoints: vec![asio, wasapi.clone(), directsound.clone()],
            completed_backends: vec![
                AudioBackend::Asio,
                AudioBackend::Wasapi,
                AudioBackend::DirectSound,
            ],
            errors: Vec::new(),
        });

        // The periodic scan deliberately does not enumerate ASIO.
        let disconnected = registry.apply_scan(DeviceScanResult {
            endpoints: Vec::new(),
            completed_backends: vec![AudioBackend::Wasapi, AudioBackend::DirectSound],
            errors: Vec::new(),
        });

        assert_eq!(disconnected.disconnected, 3);
        assert!(registry
            .find_by_id("asio:US-322 / US-366")
            .is_some_and(|endpoint| !endpoint.is_available()));
        assert_eq!(registry.unavailable_count(), 3);

        let reconnected = registry.apply_scan(DeviceScanResult {
            endpoints: vec![wasapi, directsound],
            completed_backends: vec![AudioBackend::Wasapi, AudioBackend::DirectSound],
            errors: Vec::new(),
        });

        assert_eq!(reconnected.reconnected, 3);
        assert!(registry
            .find_available_by_id("asio:US-322 / US-366")
            .is_some());
        assert_eq!(registry.unavailable_count(), 0);
    }

    #[test]
    fn ambiguous_asio_model_match_is_not_propagated() {
        let asio_one = endpoint(
            "asio:tascam-one",
            AudioBackend::Asio,
            "TASCAM US-366 ASIO",
            6,
            vec![2, 6],
        );
        let asio_two = endpoint(
            "asio:tascam-two",
            AudioBackend::Asio,
            "US-322 / US-366",
            6,
            vec![2, 6],
        );
        let wasapi = endpoint(
            "wasapi:{TASCAM}",
            AudioBackend::Wasapi,
            "Динамики (TASCAM US-366)",
            2,
            vec![2],
        );
        let mut registry = DeviceRegistry::from_scan(DeviceScanResult {
            endpoints: vec![asio_one, asio_two, wasapi],
            completed_backends: vec![AudioBackend::Asio, AudioBackend::Wasapi],
            errors: Vec::new(),
        });

        let changes = registry.apply_scan(DeviceScanResult {
            endpoints: Vec::new(),
            completed_backends: vec![AudioBackend::Wasapi],
            errors: Vec::new(),
        });

        assert_eq!(changes.disconnected, 1);
        assert!(registry.find_available_by_id("asio:tascam-one").is_some());
        assert!(registry.find_available_by_id("asio:tascam-two").is_some());
    }

    #[test]
    fn reconnect_watch_stays_active_after_retry_window_until_endpoint_returns() {
        let mut known = vec![KnownEndpointPresence {
            backend: AudioBackend::Wasapi,
            id: "wasapi:{TASCAM}".to_string(),
            present: true,
        }];

        let disconnected = DeviceScanResult {
            endpoints: Vec::new(),
            completed_backends: vec![AudioBackend::Wasapi],
            errors: Vec::new(),
        };
        assert_eq!(update_known_endpoint_presence(&mut known, &disconnected), 1);

        // A failed scan must not clear the known-unavailable state.
        let failed = DeviceScanResult {
            endpoints: Vec::new(),
            completed_backends: Vec::new(),
            errors: vec!["temporary WASAPI failure".to_string()],
        };
        assert_eq!(update_known_endpoint_presence(&mut known, &failed), 1);

        let reconnected = DeviceScanResult {
            endpoints: vec![endpoint(
                "wasapi:{TASCAM}",
                AudioBackend::Wasapi,
                "Speakers (TASCAM)",
                2,
                vec![2],
            )],
            completed_backends: vec![AudioBackend::Wasapi],
            errors: Vec::new(),
        };
        assert_eq!(update_known_endpoint_presence(&mut known, &reconnected), 0);
    }

    #[test]
    fn latest_scan_replaces_unconsumed_stale_snapshot() {
        let (tx, rx) = bounded::<DeviceScanResult>(1);
        let publisher_rx = rx.clone();

        let stale = DeviceScanResult {
            endpoints: Vec::new(),
            completed_backends: vec![AudioBackend::Wasapi],
            errors: Vec::new(),
        };
        tx.try_send(stale).unwrap();

        let latest = DeviceScanResult {
            endpoints: vec![endpoint(
                "wasapi:{TASCAM}",
                AudioBackend::Wasapi,
                "Speakers (TASCAM)",
                2,
                vec![2],
            )],
            completed_backends: vec![AudioBackend::Wasapi],
            errors: Vec::new(),
        };

        assert!(publish_latest_scan_result(
            &tx,
            &publisher_rx,
            latest,
            "test"
        ));
        let received = rx.try_recv().unwrap();
        assert_eq!(received.endpoints.len(), 1);
        assert_eq!(received.endpoints[0].id, "wasapi:{TASCAM}");
    }

    #[test]
    fn hardware_matching_ignores_generic_vendor_tokens() {
        assert!(!shares_hardware_model_token(
            "Focusrite USB ASIO",
            "Speakers (Focusrite USB Audio)"
        ));
        assert!(shares_hardware_model_token(
            "US-322 / US-366",
            "Динамики (TASCAM US-366)"
        ));
    }
}
