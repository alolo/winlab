//! Child-process host and versioned JSON-lines IPC for the audio engine.
//!
//! The GUI process owns the public `AudioEngine` facade. The selected backend,
//! native driver, Rodio mixers and decoder workers live in a child process.
//! Only control messages and playback-state updates cross this boundary; PCM
//! audio is always decoded and rendered inside the worker process.

use crate::audio::device_registry::AudioEndpoint;
use crate::audio::engine::{AudioCommand, AudioEngine, AudioPlayRequest, PositionUpdate};
use crate::audio::OutputChannelInfo;
use crate::config::{AudioBackend, AudioPerformanceSettings};
use crate::cue::{ChannelRoute, CueState, FadeCurve, OutputChannelSetting, TimePitchSettings};
use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

pub(crate) const WORKER_PROTOCOL_VERSION: u32 = 5;
const WORKER_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const WORKER_FORCE_EXIT_TIMEOUT: Duration = Duration::from_millis(500);
const WORKER_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const WORKER_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
const WORKER_HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// Includes every scheduled recovery check, even when the endpoint is still
/// unavailable and no Worker process can safely be created.
const MAX_AUTOMATIC_RECOVERY_ATTEMPTS: u8 = 5;
/// Delays from recovery-cycle start / previous attempt to attempts 1..=5.
/// The cumulative window is approximately 19 seconds.
const RECOVERY_ATTEMPT_DELAYS_SECS: [u64; 5] = [1, 1, 2, 5, 10];

fn unix_micros_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(u64::MAX as u128) as u64
}

#[cfg(target_os = "windows")]
mod windows_worker_job {
    use std::ffi::c_void;
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::ptr::{null, null_mut};

    #[repr(C)]
    struct BasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    struct ExtendedLimitInformation {
        basic_limit_information: BasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(job_attributes: *mut c_void, name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(
            job: *mut c_void,
            information_class: i32,
            information: *mut c_void,
            information_length: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn TerminateJobObject(job: *mut c_void, exit_code: u32) -> i32;
        fn CloseHandle(object: *mut c_void) -> i32;
    }

    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;

    pub fn create() -> io::Result<usize> {
        let handle = unsafe { CreateJobObjectW(null_mut(), null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }

        let mut limits: ExtendedLimitInformation = unsafe { zeroed() };
        limits.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                (&mut limits as *mut ExtendedLimitInformation).cast(),
                size_of::<ExtendedLimitInformation>() as u32,
            )
        };
        if configured == 0 {
            let error = io::Error::last_os_error();
            unsafe {
                CloseHandle(handle);
            }
            return Err(error);
        }

        Ok(handle as usize)
    }

    pub fn assign(job: usize, child: &Child) -> io::Result<()> {
        let assigned = unsafe {
            AssignProcessToJobObject(job as *mut c_void, child.as_raw_handle() as *mut c_void)
        };
        if assigned == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn terminate(job: usize) -> io::Result<()> {
        let terminated = unsafe { TerminateJobObject(job as *mut c_void, 0xC000_013A) };
        if terminated == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn close(job: usize) {
        if job != 0 {
            unsafe {
                CloseHandle(job as *mut c_void);
            }
        }
    }
}

/// Per-process Windows Job Object. Each audio worker gets its own job so a
/// stuck endpoint can be terminated without interrupting healthy endpoints.
#[derive(Debug)]
pub(crate) struct AudioWorkerJob {
    #[cfg(target_os = "windows")]
    handle: usize,
}

impl AudioWorkerJob {
    fn new() -> Option<Arc<Self>> {
        #[cfg(target_os = "windows")]
        {
            match windows_worker_job::create() {
                Ok(handle) => Some(Arc::new(Self { handle })),
                Err(error) => {
                    warn!(
                        "Could not create/configure Windows audio-worker Job Object; orphan protection is unavailable: {}",
                        error
                    );
                    None
                }
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            None
        }
    }

    fn assign_process(&self, child: &Child) -> Result<()> {
        #[cfg(target_os = "windows")]
        windows_worker_job::assign(self.handle, child)
            .map_err(|error| anyhow!("AssignProcessToJobObject failed: {error}"))?;

        #[cfg(not(target_os = "windows"))]
        let _ = child;

        Ok(())
    }

    fn terminate(&self) -> Result<()> {
        #[cfg(target_os = "windows")]
        windows_worker_job::terminate(self.handle)
            .map_err(|error| anyhow!("TerminateJobObject failed: {error}"))?;

        Ok(())
    }
}

#[cfg(target_os = "windows")]
impl Drop for AudioWorkerJob {
    fn drop(&mut self) {
        windows_worker_job::close(self.handle);
        self.handle = 0;
    }
}

/// Weak registry of child jobs owned by one AudioEngine. It lets shutdown
/// terminate every worker if the manager thread itself exceeds its deadline.
#[derive(Debug, Default)]
pub(crate) struct AudioWorkerJobSupervisor {
    jobs: Mutex<Vec<std::sync::Weak<AudioWorkerJob>>>,
}

impl AudioWorkerJobSupervisor {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn register(&self, job: &Arc<AudioWorkerJob>) {
        let mut jobs = self.jobs.lock();
        jobs.retain(|existing| existing.strong_count() > 0);
        jobs.push(Arc::downgrade(job));
    }

    pub(crate) fn terminate_all(&self) {
        let active_jobs: Vec<Arc<AudioWorkerJob>> = {
            let mut jobs = self.jobs.lock();
            let active: Vec<Arc<AudioWorkerJob>> =
                jobs.iter().filter_map(std::sync::Weak::upgrade).collect();
            jobs.retain(|existing| existing.strong_count() > 0);
            active
        };

        info!(
            "Force-terminating {} isolated audio worker process(es) through their Job Objects",
            active_jobs.len()
        );
        for job in active_jobs {
            if let Err(error) = job.terminate() {
                warn!(
                    "Could not force-terminate an audio worker Job Object: {:#}",
                    error
                );
            }
        }
    }
}

fn default_full_device_metadata() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkerStartupConfig {
    pub protocol_version: u32,
    pub backend: AudioBackend,
    pub selected_device_id: Option<String>,
    pub selected_device: Option<String>,
    pub performance: AudioPerformanceSettings,
    /// Full device enumeration is needed by the global worker's settings UI.
    /// Per-Cue endpoint workers only need metadata for their selected endpoint;
    /// skipping unrelated ASIO input/output enumeration reduces cold-start time.
    #[serde(default = "default_full_device_metadata")]
    pub include_full_device_metadata: bool,
    /// `None` asks the child to perform a fresh full endpoint scan. `Some`
    /// preserves the parent's known online/offline endpoint snapshot.
    pub registry_endpoints: Option<Vec<AudioEndpoint>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
enum WorkerRequest {
    Initialize(WorkerStartupConfig),
    Command {
        protocol_version: u32,
        request_id: u64,
        command: WorkerCommand,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", content = "payload")]
enum WorkerCommand {
    Play {
        id: Uuid,
        generation: u64,
        file: PathBuf,
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
        loop_count: i32,
    },
    PlayBatch {
        transaction_id: Uuid,
        cues: Vec<AudioPlayRequest>,
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
        performance: AudioPerformanceSettings,
    },
    ReconfigureOutput {
        performance: AudioPerformanceSettings,
    },
    SetInputDevice(String),
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
    Shutdown,
}

fn audio_play_request_from_command(
    command: AudioCommand,
) -> Result<AudioPlayRequest, AudioCommand> {
    match command {
        AudioCommand::Play {
            id,
            generation,
            file,
            output_device: _,
            output_backend: _,
            output_device_id: _,
            start_secs,
            end_secs,
            duration_secs,
            volume,
            volume_automation_enabled,
            volume_automation,
            time_pitch,
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
        } => Ok(AudioPlayRequest {
            id,
            generation,
            file,
            start_secs,
            end_secs,
            duration_secs,
            volume,
            volume_automation_enabled,
            volume_automation,
            time_pitch,
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
        }),
        other => Err(other),
    }
}

/// Collects Play requests only between explicit GUI launch-transaction markers.
/// There is deliberately no wall-clock coalescing window: unrelated commands
/// must never be merged merely because they happened to arrive close together.
#[derive(Default)]
struct LaunchBatchAccumulator {
    active_transaction_id: Option<Uuid>,
    cues: Vec<AudioPlayRequest>,
}

impl LaunchBatchAccumulator {
    fn is_active(&self) -> bool {
        self.active_transaction_id.is_some()
    }

    /// Starts a transaction. If a previous transaction was left open, return
    /// its pending cues so the caller can flush them before starting the new one.
    fn begin(&mut self, transaction_id: Uuid) -> Option<(Uuid, Vec<AudioPlayRequest>)> {
        let previous = self.finish_active();
        self.active_transaction_id = Some(transaction_id);
        previous
    }

    fn push(&mut self, cue: AudioPlayRequest) -> bool {
        if self.active_transaction_id.is_none() {
            return false;
        }
        self.cues.push(cue);
        true
    }

    /// Flushes the current contiguous Play segment but leaves the transaction
    /// open. This preserves command ordering if Stop/Fade/volume arrives between
    /// two portions of a transaction.
    fn take_pending(&mut self) -> Option<(Uuid, Vec<AudioPlayRequest>)> {
        let transaction_id = self.active_transaction_id?;
        if self.cues.is_empty() {
            return None;
        }
        Some((transaction_id, std::mem::take(&mut self.cues)))
    }

    fn finish(
        &mut self,
        transaction_id: Uuid,
    ) -> Result<Option<(Uuid, Vec<AudioPlayRequest>)>, Uuid> {
        match self.active_transaction_id {
            Some(active_id) if active_id == transaction_id => Ok(self.finish_active()),
            Some(active_id) => Err(active_id),
            None => Ok(None),
        }
    }

    fn finish_active(&mut self) -> Option<(Uuid, Vec<AudioPlayRequest>)> {
        let transaction_id = self.active_transaction_id.take()?;
        if self.cues.is_empty() {
            None
        } else {
            Some((transaction_id, std::mem::take(&mut self.cues)))
        }
    }
}

fn worker_play_command_from_request(cue: AudioPlayRequest) -> WorkerCommand {
    WorkerCommand::Play {
        id: cue.id,
        generation: cue.generation,
        file: cue.file,
        output_device: None,
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
    }
}

fn worker_command_for_launch_segment(
    transaction_id: Uuid,
    cues: Vec<AudioPlayRequest>,
) -> WorkerCommand {
    // A one-Cue transaction stays on the ordinary Play path. Only genuinely
    // simultaneous groups use sample-frame scheduling and its preparation pre-roll.
    if cues.len() == 1 {
        worker_play_command_from_request(cues.into_iter().next().expect("one cue"))
    } else {
        WorkerCommand::PlayBatch {
            transaction_id,
            cues,
        }
    }
}

fn write_play_batch_request(
    writer: &mut impl Write,
    worker_label: &str,
    transaction_id: Uuid,
    cues: Vec<AudioPlayRequest>,
) -> bool {
    if cues.is_empty() {
        return true;
    }

    let cue_ids: Vec<Uuid> = cues.iter().map(|cue| cue.id).collect();
    let cue_count = cues.len();
    let command = worker_command_for_launch_segment(transaction_id, cues);
    let request = WorkerRequest::Command {
        protocol_version: WORKER_PROTOCOL_VERSION,
        request_id: 0,
        command,
    };

    match write_json_line(writer, &request) {
        Ok(()) => {
            if cue_count == 1 {
                debug!(
                    "Explicit launch transaction sent as direct Play (no batch pre-roll): transaction_id={}, cue_id={}, worker={}",
                    transaction_id,
                    cue_ids[0],
                    worker_label
                );
            } else {
                info!(
                    "Explicit launch transaction sent as one Worker PlayBatch: transaction_id={}, cues={}, cue_ids={:?}, worker={}",
                    transaction_id,
                    cue_count,
                    cue_ids,
                    worker_label
                );
            }
            true
        }
        Err(error) => {
            error!(
                "Could not write explicit launch transaction to audio worker '{}': {}",
                worker_label, error
            );
            false
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkerEngineMetadata {
    pub backend: AudioBackend,
    pub output_devices: Vec<String>,
    pub registry_endpoints: Vec<AudioEndpoint>,
    pub input_devices: Vec<String>,
    pub current_device: String,
    pub current_device_id: String,
    pub current_input_device: String,
    pub output_channels: usize,
    pub current_sample_rate: u32,
    pub performance_settings: AudioPerformanceSettings,
    pub supported_sample_rates: Vec<u32>,
    pub supported_buffer_sizes: Vec<u32>,
    pub output_device_channels: HashMap<String, usize>,
    pub output_device_channel_layouts: HashMap<String, Vec<usize>>,
    pub output_driver_channel_info: HashMap<String, Vec<OutputChannelInfo>>,
    pub detected_default_output_pairs: HashMap<String, [usize; 2]>,
}

#[derive(Debug, Clone)]
pub(crate) enum AudioWorkerManagerEvent {
    /// Aggregated peak level from all currently running output endpoint workers.
    OutputMeter { peak: f32 },
    EndpointLost {
        backend: AudioBackend,
        endpoint_id: String,
        device_name: String,
        is_global: bool,
        affected_cues: usize,
    },
    EndpointRecovered {
        metadata: WorkerEngineMetadata,
        is_global: bool,
    },
    EndpointRecoveryFailed {
        backend: AudioBackend,
        endpoint_id: String,
        device_name: String,
        is_global: bool,
        error: String,
        retry_scheduled: bool,
    },
    /// Lightweight process/heartbeat snapshot, emitted once per second.
    PerformanceSnapshot {
        worker_pids: Vec<u32>,
        heartbeat_max_age_ms: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkerOutputState {
    pub channels: usize,
    pub sample_rate: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "payload")]
enum WorkerEvent {
    Ready {
        protocol_version: u32,
        result: std::result::Result<WorkerEngineMetadata, String>,
    },
    Position(PositionUpdate),
    OutputMeter {
        peak: f32,
    },
    Response {
        request_id: u64,
        result: std::result::Result<WorkerOutputState, String>,
    },
    ProtocolError {
        message: String,
    },
    Heartbeat {
        sequence: u64,
    },
    Stopped,
}

#[derive(Debug)]
struct WorkerHealth {
    last_heartbeat: Instant,
    heartbeat_sequence: u64,
    event_stream_closed: bool,
    shutdown_acknowledged: bool,
    /// True when the manager deliberately terminates this process after endpoint loss.
    expected_termination: bool,
}

impl WorkerHealth {
    fn new() -> Self {
        Self {
            last_heartbeat: Instant::now(),
            heartbeat_sequence: 0,
            event_stream_closed: false,
            shutdown_acknowledged: false,
            expected_termination: false,
        }
    }

    fn record_heartbeat(&mut self, sequence: u64) {
        self.last_heartbeat = Instant::now();
        self.heartbeat_sequence = sequence;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerHealthIssue {
    HeartbeatTimeout,
    EventStreamClosed,
}

impl WorkerHealthIssue {
    fn description(self) -> &'static str {
        match self {
            Self::HeartbeatTimeout => "heartbeat timeout",
            Self::EventStreamClosed => "event stream closed unexpectedly",
        }
    }
}

fn worker_health_issue(health: &WorkerHealth, now: Instant) -> Option<WorkerHealthIssue> {
    if health.shutdown_acknowledged {
        None
    } else if health.event_stream_closed {
        Some(WorkerHealthIssue::EventStreamClosed)
    } else if now.saturating_duration_since(health.last_heartbeat) >= WORKER_HEARTBEAT_TIMEOUT {
        Some(WorkerHealthIssue::HeartbeatTimeout)
    } else {
        None
    }
}

pub(crate) struct SpawnedAudioWorker {
    pub metadata: WorkerEngineMetadata,
    pub command_tx: Sender<AudioCommand>,
    pub position_rx: Receiver<PositionUpdate>,
    pub output_meter_rx: Receiver<f32>,
    pub process: AudioWorkerProcess,
    health: Arc<Mutex<WorkerHealth>>,
}

pub(crate) struct AudioWorkerProcess {
    child: Option<Child>,
    worker_label: String,
    job: Option<Arc<AudioWorkerJob>>,
    expected_forced_exit: bool,
}

impl AudioWorkerProcess {
    fn new(child: Child, worker_label: String, job: Option<Arc<AudioWorkerJob>>) -> Self {
        Self {
            child: Some(child),
            worker_label,
            job,
            expected_forced_exit: false,
        }
    }

    /// Request immediate process termination after a confirmed endpoint loss.
    /// The shared health state is marked first so the IPC dispatcher does not
    /// report this deliberate termination as an unexpected worker crash.
    fn request_force_termination(&mut self, health: &Arc<Mutex<WorkerHealth>>) -> Result<()> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };

        if let Ok(Some(_)) = child.try_wait() {
            return Ok(());
        }

        health.lock().expected_termination = true;

        let termination_result = if let Some(job) = self.job.as_deref() {
            if let Err(job_error) = job.terminate() {
                warn!(
                    "Could not terminate audio worker '{}' through its Job Object: {:#}; falling back to Child::kill",
                    self.worker_label, job_error
                );
                child
                    .kill()
                    .with_context(|| format!("Could not kill audio worker '{}'", self.worker_label))
            } else {
                Ok(())
            }
        } else {
            child
                .kill()
                .with_context(|| format!("Could not kill audio worker '{}'", self.worker_label))
        };

        if let Err(error) = termination_result {
            health.lock().expected_termination = false;
            return Err(error);
        }

        self.expected_forced_exit = true;
        Ok(())
    }
}

fn terminate_child_bounded(child: &mut Child, job: Option<&AudioWorkerJob>, worker_label: &str) {
    if let Some(job) = job {
        if let Err(error) = job.terminate() {
            warn!(
                "Could not terminate audio worker '{}' through its Job Object: {:#}; falling back to Child::kill",
                worker_label, error
            );
            if let Err(kill_error) = child.kill() {
                warn!(
                    "Could not send force-termination request to audio worker '{}': {}",
                    worker_label, kill_error
                );
            }
        }
    } else if let Err(error) = child.kill() {
        warn!(
            "Could not send force-termination request to audio worker '{}': {}",
            worker_label, error
        );
    }

    let deadline = Instant::now() + WORKER_FORCE_EXIT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                info!(
                    "Audio worker '{}' terminated after forced shutdown: {}",
                    worker_label, status
                );
                return;
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                error!(
                    "Audio worker '{}' did not confirm process exit within {:?}; its kill-on-close Job Object will enforce cleanup",
                    worker_label, WORKER_FORCE_EXIT_TIMEOUT
                );
                return;
            }
            Err(error) => {
                warn!(
                    "Could not confirm forced exit of audio worker '{}': {}",
                    worker_label, error
                );
                return;
            }
        }
    }
}

impl Drop for AudioWorkerProcess {
    fn drop(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };

        info!(
            "Waiting up to {:?} for audio worker '{}' to exit",
            WORKER_SHUTDOWN_TIMEOUT, self.worker_label
        );
        let deadline = Instant::now() + WORKER_SHUTDOWN_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if status.success() {
                        info!(
                            "Audio worker '{}' exited cleanly: {}",
                            self.worker_label, status
                        );
                    } else if self.expected_forced_exit {
                        info!(
                            "Audio worker '{}' exited after expected endpoint-loss termination: {}",
                            self.worker_label, status
                        );
                    } else {
                        warn!(
                            "Audio worker '{}' exited with a failure status: {}",
                            self.worker_label, status
                        );
                    }
                    self.child = None;
                    return;
                }
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    warn!(
                        "Audio worker '{}' did not stop within {:?}; forcing endpoint process termination",
                        self.worker_label, WORKER_SHUTDOWN_TIMEOUT
                    );
                    terminate_child_bounded(child, self.job.as_deref(), &self.worker_label);
                    self.child = None;
                    return;
                }
                Err(error) => {
                    warn!(
                        "Could not inspect audio worker '{}' exit status: {}; forcing endpoint process termination",
                        self.worker_label, error
                    );
                    terminate_child_bounded(child, self.job.as_deref(), &self.worker_label);
                    self.child = None;
                    return;
                }
            }
        }
    }
}

type PendingReplies = Arc<Mutex<HashMap<u64, Sender<std::result::Result<(usize, u32), String>>>>>;

pub(crate) fn spawn_audio_worker(
    mut config: WorkerStartupConfig,
    job_supervisor: Arc<AudioWorkerJobSupervisor>,
) -> Result<SpawnedAudioWorker> {
    let worker_startup_started = Instant::now();
    let requested_performance = config.performance;
    config.performance =
        AudioPerformanceSettings::for_backend(config.backend, requested_performance);
    if config.performance != requested_performance {
        info!(
            "Ignoring TaSQ performance override for ASIO endpoint '{}'; the driver controls sample rate and hardware buffer",
            config.selected_device.as_deref().unwrap_or("default output")
        );
    }
    let executable = std::env::current_exe()
        .context("Could not locate the TaSQ executable for the audio worker process")?;
    let worker_label = format!(
        "{} — {} [{}]",
        config.backend.short_label(),
        config
            .selected_device
            .as_deref()
            .unwrap_or("System Default"),
        config.selected_device_id.as_deref().unwrap_or("default")
    );

    info!(
        "Starting isolated audio worker process: executable='{}', endpoint='{}', protocol={}",
        executable.display(),
        worker_label,
        WORKER_PROTOCOL_VERSION
    );

    let mut command = Command::new(&executable);
    command
        .arg("--tasq-audio-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("Could not launch audio worker '{}'", executable.display()))?;
    let worker_job = AudioWorkerJob::new().and_then(|job| match job.assign_process(&child) {
        Ok(()) => {
            job_supervisor.register(&job);
            info!(
                "Audio worker '{}' (pid={}) assigned to a kill-on-close Job Object",
                worker_label,
                child.id()
            );
            Some(job)
        }
        Err(error) => {
            warn!(
                "Could not assign audio worker '{}' (pid={}) to its kill-on-close Job Object; direct process termination will be used as fallback: {:#}",
                worker_label,
                child.id(),
                error
            );
            None
        }
    });
    info!(
        "Audio worker '{}' started with PID {}",
        worker_label,
        child.id()
    );
    let child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("Audio worker stdin pipe was not created"))?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Audio worker stdout pipe was not created"))?;
    let child_stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Audio worker stderr pipe was not created"))?;

    let (event_tx, event_rx) = unbounded::<WorkerEvent>();
    let stdout_label = worker_label.clone();
    thread::Builder::new()
        .name("tasq-audio-worker-stdout".to_string())
        .spawn(move || {
            let reader = BufReader::new(child_stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) if line.trim().is_empty() => {}
                    Ok(line) => match serde_json::from_str::<WorkerEvent>(&line) {
                        Ok(event) => {
                            if event_tx.send(event).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = event_tx.send(WorkerEvent::ProtocolError {
                                message: format!(
                                    "Invalid IPC JSON from worker '{}': {}; line={}",
                                    stdout_label, error, line
                                ),
                            });
                        }
                    },
                    Err(error) => {
                        let _ = event_tx.send(WorkerEvent::ProtocolError {
                            message: format!(
                                "Could not read stdout from worker '{}': {}",
                                stdout_label, error
                            ),
                        });
                        break;
                    }
                }
            }
        })
        .context("Could not start audio worker stdout reader")?;

    let stderr_label = worker_label.clone();
    thread::Builder::new()
        .name("tasq-audio-worker-stderr".to_string())
        .spawn(move || {
            let reader = BufReader::new(child_stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) => info!(target: "TaSQ::audio::worker::child", worker = %stderr_label, "{}", line),
                    Err(error) => {
                        warn!(
                            target: "TaSQ::audio::worker::child",
                            worker = %stderr_label,
                            "Could not read worker stderr: {}",
                            error
                        );
                        break;
                    }
                }
            }
        })
        .context("Could not start audio worker stderr reader")?;

    let mut writer = BufWriter::new(child_stdin);
    write_json_line(&mut writer, &WorkerRequest::Initialize(config.clone()))
        .context("Could not send audio worker initialization request")?;

    let ready = match event_rx.recv_timeout(WORKER_STARTUP_TIMEOUT) {
        Ok(WorkerEvent::Ready {
            protocol_version,
            result,
        }) => {
            if protocol_version != WORKER_PROTOCOL_VERSION {
                terminate_child_bounded(&mut child, worker_job.as_deref(), &worker_label);
                anyhow::bail!(
                    "Audio worker protocol mismatch: parent={}, child={}",
                    WORKER_PROTOCOL_VERSION,
                    protocol_version
                );
            }
            match result {
                Ok(metadata) => metadata,
                Err(error) => {
                    terminate_child_bounded(&mut child, worker_job.as_deref(), &worker_label);
                    anyhow::bail!(
                        "Audio worker '{}' could not initialize: {}",
                        worker_label,
                        error
                    );
                }
            }
        }
        Ok(other) => {
            terminate_child_bounded(&mut child, worker_job.as_deref(), &worker_label);
            anyhow::bail!(
                "Audio worker '{}' sent an unexpected startup event: {:?}",
                worker_label,
                other
            );
        }
        Err(error) => {
            terminate_child_bounded(&mut child, worker_job.as_deref(), &worker_label);
            anyhow::bail!(
                "Audio worker '{}' did not initialize within {:?}: {}",
                worker_label,
                WORKER_STARTUP_TIMEOUT,
                error
            );
        }
    };
    info!(
        "Audio worker '{}' completed its initialization handshake in {:.1} ms (pid={}, endpoint sample_rate={} Hz, channels={})",
        worker_label,
        worker_startup_started.elapsed().as_secs_f64() * 1_000.0,
        child.id(),
        ready.current_sample_rate,
        ready.output_channels
    );

    let (command_tx, command_rx) = bounded::<AudioCommand>(256);
    let (position_tx, position_rx) = bounded::<PositionUpdate>(1024);
    let (output_meter_tx, output_meter_rx) = bounded::<f32>(8);
    let pending_replies: PendingReplies = Arc::new(Mutex::new(HashMap::new()));
    let next_request_id = Arc::new(AtomicU64::new(1));
    let health = Arc::new(Mutex::new(WorkerHealth::new()));

    let dispatcher_pending = pending_replies.clone();
    let dispatcher_health = health.clone();
    let dispatcher_label = worker_label.clone();
    thread::Builder::new()
        .name("tasq-audio-worker-events".to_string())
        .spawn(move || {
            let mut stopped_cleanly = false;
            while let Ok(event) = event_rx.recv() {
                match event {
                    WorkerEvent::Position(update) => {
                        let _ = position_tx.try_send(update);
                    }
                    WorkerEvent::OutputMeter { peak } => {
                        let peak = if peak.is_finite() { peak.max(0.0) } else { 0.0 };
                        let _ = output_meter_tx.try_send(peak);
                    }
                    WorkerEvent::Response { request_id, result } => {
                        if let Some(reply) = dispatcher_pending.lock().remove(&request_id) {
                            let result = result.map(|state| (state.channels, state.sample_rate));
                            let _ = reply.send(result);
                        } else {
                            warn!(
                                "Audio worker '{}' returned unknown request ID {}",
                                dispatcher_label, request_id
                            );
                        }
                    }
                    WorkerEvent::ProtocolError { message } => {
                        warn!("Audio worker '{}': {}", dispatcher_label, message);
                    }
                    WorkerEvent::Heartbeat { sequence } => {
                        dispatcher_health.lock().record_heartbeat(sequence);
                    }
                    WorkerEvent::Stopped => {
                        info!("Audio worker '{}' acknowledged shutdown", dispatcher_label);
                        stopped_cleanly = true;
                        break;
                    }
                    WorkerEvent::Ready { .. } => {
                        warn!(
                            "Audio worker '{}' sent a duplicate Ready event",
                            dispatcher_label
                        );
                    }
                }
            }

            let expected_termination = {
                let mut health = dispatcher_health.lock();
                health.event_stream_closed = true;
                let expected = health.expected_termination;
                health.shutdown_acknowledged = stopped_cleanly || expected;
                expected
            };

            for (_, reply) in dispatcher_pending.lock().drain() {
                let _ = reply.send(Err(format!(
                    "Audio worker '{}' closed its IPC channel",
                    dispatcher_label
                )));
            }
            if stopped_cleanly {
                debug!(
                    "Audio worker '{}' event channel closed after clean shutdown",
                    dispatcher_label
                );
            } else if expected_termination {
                debug!(
                    "Audio worker '{}' event channel closed after expected endpoint-loss termination",
                    dispatcher_label
                );
            } else {
                error!(
                    "Audio worker '{}' event channel closed unexpectedly",
                    dispatcher_label
                );
            }
        })
        .context("Could not start audio worker event dispatcher")?;

    let command_label = worker_label.clone();
    thread::Builder::new()
        .name("tasq-audio-worker-commands".to_string())
        .spawn(move || {
            let mut writer = writer;
            let mut launch_batch = LaunchBatchAccumulator::default();
            loop {
                let received = command_rx.recv();
                let command = match received {
                    Ok(command) => command,
                    Err(_) => {
                        if let Some((transaction_id, cues)) = launch_batch.finish_active() {
                            warn!(
                                "Worker command channel closed before launch transaction {} ended; flushing {} queued Cue(s)",
                                transaction_id,
                                cues.len()
                            );
                            if !write_play_batch_request(
                                &mut writer,
                                &command_label,
                                transaction_id,
                                cues,
                            ) {
                                break;
                            }
                        }
                        break;
                    }
                };

                let command = match command {
                    AudioCommand::BeginLaunchTransaction { transaction_id } => {
                        if let Some((previous_id, cues)) = launch_batch.begin(transaction_id) {
                            warn!(
                                "A new launch transaction {} arrived before transaction {} ended; flushing {} queued Cue(s)",
                                transaction_id,
                                previous_id,
                                cues.len()
                            );
                            if !write_play_batch_request(
                                &mut writer,
                                &command_label,
                                previous_id,
                                cues,
                            ) {
                                break;
                            }
                        }
                        debug!(
                            "Worker entered explicit launch transaction {} ({})",
                            transaction_id,
                            command_label
                        );
                        continue;
                    }
                    AudioCommand::EndLaunchTransaction { transaction_id } => {
                        match launch_batch.finish(transaction_id) {
                            Ok(Some((finished_id, cues))) => {
                                if !write_play_batch_request(
                                    &mut writer,
                                    &command_label,
                                    finished_id,
                                    cues,
                                ) {
                                    break;
                                }
                            }
                            Ok(None) => {
                                debug!(
                                    "Worker launch transaction {} ended without queued Play commands ({})",
                                    transaction_id,
                                    command_label
                                );
                            }
                            Err(active_id) => {
                                error!(
                                    "Worker received mismatched launch transaction end: requested={}, active={}; flushing active transaction",
                                    transaction_id,
                                    active_id
                                );
                                if let Some((finished_id, cues)) = launch_batch.finish_active() {
                                    if !write_play_batch_request(
                                        &mut writer,
                                        &command_label,
                                        finished_id,
                                        cues,
                                    ) {
                                        break;
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    play @ AudioCommand::Play { .. } if launch_batch.is_active() => {
                        match audio_play_request_from_command(play) {
                            Ok(cue) => {
                                let cue_id = cue.id;
                                if !launch_batch.push(cue) {
                                    error!(
                                        "Could not buffer Cue {}: launch transaction was not active",
                                        cue_id
                                    );
                                }
                            }
                            Err(_) => unreachable!("Play pattern must convert to AudioPlayRequest"),
                        }
                        continue;
                    }
                    other => other,
                };

                // Preserve ordering around Stop/Fade/volume/configuration commands.
                // Consecutive Play commands inside the explicit transaction remain
                // one batch; a non-Play command forms a deliberate boundary.
                if let Some((transaction_id, cues)) = launch_batch.take_pending() {
                    if !write_play_batch_request(
                        &mut writer,
                        &command_label,
                        transaction_id,
                        cues,
                    ) {
                        break;
                    }
                }

                let request_id = match &command {
                    AudioCommand::SetOutputDevice { .. }
                    | AudioCommand::ReconfigureOutput { .. } => {
                        next_request_id.fetch_add(1, Ordering::Relaxed)
                    }
                    _ => 0,
                };

                let is_shutdown = matches!(&command, &AudioCommand::Shutdown);
                let worker_command = match command {
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
                        volume_automation,
                        time_pitch,
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
                    } => WorkerCommand::Play {
                        id,
                        generation,
                        file,
                        output_device,
                        start_secs,
                        end_secs,
                        duration_secs,
                        volume,
                        volume_automation_enabled,
                        volume_automation,
                        time_pitch,
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
                    },
                    AudioCommand::PlayBatch { transaction_id, cues } => {
                        WorkerCommand::PlayBatch { transaction_id, cues }
                    },
                    AudioCommand::Stop(id) => WorkerCommand::Stop(id),
                    AudioCommand::StopTimed { id, request_id, requested_at_unix_micros } => {
                        WorkerCommand::StopTimed { id, request_id, requested_at_unix_micros }
                    },
                    AudioCommand::StopAll => WorkerCommand::StopAll,
                    AudioCommand::StopAllTimed { request_id, requested_at_unix_micros } => {
                        WorkerCommand::StopAllTimed { request_id, requested_at_unix_micros }
                    },
                    AudioCommand::Pause(id) => WorkerCommand::Pause(id),
                    AudioCommand::Seek { id, position_secs } => {
                        WorkerCommand::Seek { id, position_secs }
                    }
                    AudioCommand::PauseAll => WorkerCommand::PauseAll,
                    AudioCommand::Resume(id) => WorkerCommand::Resume(id),
                    AudioCommand::ResumeAll => WorkerCommand::ResumeAll,
                    AudioCommand::SetVolume(id, volume) => WorkerCommand::SetVolume(id, volume),
                    AudioCommand::SetVolumeAutomation {
                        id,
                        enabled,
                        points,
                    } => WorkerCommand::SetVolumeAutomation {
                        id,
                        enabled,
                        points,
                    },
                    AudioCommand::SetTimePitch { id, settings } => {
                        WorkerCommand::SetTimePitch { id, settings }
                    }
                    AudioCommand::Fade {
                        id,
                        target_volume,
                        duration_secs,
                        stop_on_finish,
                        curve,
                        custom_fade_points,
                    } => WorkerCommand::Fade {
                        id,
                        target_volume,
                        duration_secs,
                        stop_on_finish,
                        curve,
                        custom_fade_points,
                    },
                    AudioCommand::SetOutputDevice {
                        device_name,
                        endpoint_id: _,
                        performance,
                        reply,
                    } => {
                        pending_replies.lock().insert(request_id, reply);
                        WorkerCommand::SetOutputDevice {
                            device_name,
                            performance,
                        }
                    }
                    AudioCommand::ReconfigureOutput { performance, reply } => {
                        pending_replies.lock().insert(request_id, reply);
                        WorkerCommand::ReconfigureOutput { performance }
                    }
                    AudioCommand::SetInputDevice(name) => WorkerCommand::SetInputDevice(name),
                    AudioCommand::TestOutput {
                        device_name,
                        logical_channel,
                        channel,
                    } => WorkerCommand::TestOutput {
                        device_name,
                        logical_channel,
                        channel,
                    },
                    AudioCommand::SetRouting { id, routes } => {
                        WorkerCommand::SetRouting { id, routes }
                    }
                    AudioCommand::SetOutputGains { id, gains_db } => {
                        WorkerCommand::SetOutputGains { id, gains_db }
                    }
                    AudioCommand::SetOutputChannelSettings {
                        device_name,
                        settings,
                    } => WorkerCommand::SetOutputChannelSettings {
                        device_name,
                        settings,
                    },
                    AudioCommand::BeginLaunchTransaction { .. }
                    | AudioCommand::EndLaunchTransaction { .. }
                    | AudioCommand::SetEndpointPreferences { .. }
                    | AudioCommand::UpdateDeviceRegistry { .. } => {
                        warn!("Manager-only audio command reached child IPC writer; ignoring it");
                        continue;
                    }
                    AudioCommand::Shutdown => WorkerCommand::Shutdown,
                };

                let request = WorkerRequest::Command {
                    protocol_version: WORKER_PROTOCOL_VERSION,
                    request_id,
                    command: worker_command,
                };
                if let Err(error) = write_json_line(&mut writer, &request) {
                    if request_id != 0 {
                        if let Some(reply) = pending_replies.lock().remove(&request_id) {
                            let _ = reply.send(Err(format!(
                                "Could not send command to audio worker '{}': {}",
                                command_label, error
                            )));
                        }
                    }
                    error!(
                        "Could not write command to audio worker '{}': {}",
                        command_label, error
                    );
                    break;
                }

                if is_shutdown {
                    break;
                }
            }
        })
        .context("Could not start audio worker command writer")?;

    Ok(SpawnedAudioWorker {
        metadata: ready,
        command_tx,
        position_rx,
        output_meter_rx,
        process: AudioWorkerProcess::new(child, worker_label, worker_job),
        health,
    })
}

/// Runs one isolated audio process per stable output endpoint. The initially
/// selected global endpoint is already running; additional endpoint workers
/// are created lazily when a Cue first targets them.
struct ManagedAudioWorker {
    metadata: WorkerEngineMetadata,
    backend: AudioBackend,
    endpoint_id: String,
    device_name: String,
    command_tx: Sender<AudioCommand>,
    position_rx: Receiver<PositionUpdate>,
    output_meter_rx: Receiver<f32>,
    latest_output_peak: f32,
    last_output_meter_at: Instant,
    _process: AudioWorkerProcess,
    pid: u32,
    output_channels: usize,
    sample_rate: u32,
    performance_settings: AudioPerformanceSettings,
    /// Last workstation profile requested for this endpoint. It can differ
    /// from the actual profile when startup had to fall back to Automatic.
    requested_performance_settings: AudioPerformanceSettings,
    health: Arc<Mutex<WorkerHealth>>,
    reported_health_issue: Option<WorkerHealthIssue>,
}

impl ManagedAudioWorker {
    fn from_spawned(
        spawned: SpawnedAudioWorker,
        endpoint_id_override: Option<&str>,
        requested_performance_settings: AudioPerformanceSettings,
    ) -> Self {
        let metadata = spawned.metadata;
        let health = spawned.health.clone();
        let pid = spawned
            .process
            .child
            .as_ref()
            .map(Child::id)
            .unwrap_or_default();
        Self {
            metadata: metadata.clone(),
            backend: metadata.backend,
            endpoint_id: endpoint_id_override
                .map(str::to_owned)
                .unwrap_or(metadata.current_device_id),
            device_name: metadata.current_device,
            command_tx: spawned.command_tx,
            position_rx: spawned.position_rx,
            output_meter_rx: spawned.output_meter_rx,
            latest_output_peak: 0.0,
            last_output_meter_at: Instant::now(),
            _process: spawned.process,
            pid,
            output_channels: metadata.output_channels.max(1),
            sample_rate: metadata.current_sample_rate,
            performance_settings: metadata.performance_settings,
            requested_performance_settings,
            health,
            reported_health_issue: None,
        }
    }
}

fn monitor_worker_health(
    workers: &mut HashMap<String, ManagedAudioWorker>,
) -> Vec<(String, WorkerHealthIssue)> {
    let now = Instant::now();
    let mut failures = Vec::new();

    for (key, worker) in workers.iter_mut() {
        let (shutdown_acknowledged, expected_termination, issue, heartbeat_sequence, heartbeat_age) = {
            let health = worker.health.lock();
            (
                health.shutdown_acknowledged,
                health.expected_termination,
                worker_health_issue(&health, now),
                health.heartbeat_sequence,
                now.saturating_duration_since(health.last_heartbeat),
            )
        };

        if shutdown_acknowledged || expected_termination {
            worker.reported_health_issue = None;
            continue;
        }

        match issue {
            Some(WorkerHealthIssue::EventStreamClosed)
                if worker.reported_health_issue != Some(WorkerHealthIssue::EventStreamClosed) =>
            {
                error!(
                    "Audio worker health failure: event stream closed unexpectedly; endpoint={} ({} — {}), pid={}, last_heartbeat_sequence={}",
                    key,
                    worker.backend.short_label(),
                    worker.device_name,
                    worker.pid,
                    heartbeat_sequence
                );
                worker.reported_health_issue = Some(WorkerHealthIssue::EventStreamClosed);
                failures.push((key.clone(), WorkerHealthIssue::EventStreamClosed));
            }
            Some(WorkerHealthIssue::HeartbeatTimeout)
                if worker.reported_health_issue != Some(WorkerHealthIssue::HeartbeatTimeout) =>
            {
                warn!(
                    "Audio worker heartbeat missing for {:.1}s (control loop may be blocked); endpoint={} ({} — {}), pid={}, last_heartbeat_sequence={}",
                    heartbeat_age.as_secs_f32(),
                    key,
                    worker.backend.short_label(),
                    worker.device_name,
                    worker.pid,
                    heartbeat_sequence
                );
                worker.reported_health_issue = Some(WorkerHealthIssue::HeartbeatTimeout);
                failures.push((key.clone(), WorkerHealthIssue::HeartbeatTimeout));
            }
            None if worker.reported_health_issue.is_some() => {
                info!(
                    "Audio worker heartbeat recovered: endpoint={} ({} — {}), pid={}, heartbeat_sequence={}",
                    key,
                    worker.backend.short_label(),
                    worker.device_name,
                    worker.pid,
                    heartbeat_sequence
                );
                worker.reported_health_issue = None;
            }
            _ => {}
        }
    }

    failures
}

fn recovery_attempt_delay(attempt: u8) -> Option<Duration> {
    let delay_index = attempt.checked_sub(1)? as usize;
    RECOVERY_ATTEMPT_DELAYS_SECS
        .get(delay_index)
        .copied()
        .map(Duration::from_secs)
}

fn mark_recovery_attempt_started(
    key: &str,
    attempts_started: &mut HashMap<String, u8>,
) -> Option<u8> {
    let attempts = attempts_started.entry(key.to_owned()).or_insert(0);
    if *attempts >= MAX_AUTOMATIC_RECOVERY_ATTEMPTS {
        return None;
    }
    *attempts += 1;
    Some(*attempts)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryRetryPlan {
    Scheduled { next_attempt: u8, delay: Duration },
    Exhausted { attempts: u8 },
}

/// Schedule the next recovery check regardless of current endpoint
/// availability. The timer is the recovery-cycle clock: when it expires, the
/// manager checks the registry and only creates a Worker if the endpoint is
/// present. An unavailable endpoint still consumes that scheduled attempt.
fn schedule_next_recovery_attempt(
    key: &str,
    attempts_started: &HashMap<String, u8>,
    retry_after: &mut HashMap<String, Instant>,
) -> RecoveryRetryPlan {
    let attempts = attempts_started.get(key).copied().unwrap_or(0);
    let next_attempt = attempts.saturating_add(1);
    let Some(delay) = recovery_attempt_delay(next_attempt) else {
        retry_after.remove(key);
        return RecoveryRetryPlan::Exhausted { attempts };
    };

    retry_after.insert(key.to_owned(), Instant::now() + delay);
    RecoveryRetryPlan::Scheduled {
        next_attempt,
        delay,
    }
}

/// A manual retry starts a fresh recovery cycle. The failed direct attempt
/// initiated by the user's action counts as attempt 1; subsequent checks use
/// the same bounded schedule as automatic recovery.
fn schedule_manual_recovery_retry(
    key: &str,
    attempts_started: &mut HashMap<String, u8>,
    retry_after: &mut HashMap<String, Instant>,
) -> (u8, RecoveryRetryPlan) {
    attempts_started.insert(key.to_owned(), 0);
    let attempt = mark_recovery_attempt_started(key, attempts_started).unwrap_or(1);
    let next = schedule_next_recovery_attempt(key, attempts_started, retry_after);
    (attempt, next)
}

/// Every fresh physical reconnect starts a new recovery cycle. Any timer
/// from the previous disconnected period is discarded and attempt 1 is
/// scheduled against the now-available endpoint.
fn schedule_recovery_after_reconnect(
    key: &str,
    attempts_started: &mut HashMap<String, u8>,
    retry_after: &mut HashMap<String, Instant>,
) -> RecoveryRetryPlan {
    attempts_started.insert(key.to_owned(), 0);
    retry_after.remove(key);
    schedule_next_recovery_attempt(key, attempts_started, retry_after)
}

fn endpoint_worker_key(backend: AudioBackend, endpoint_id: &str) -> String {
    format!("{}::{}", backend.short_label(), endpoint_id)
}

/// Explicit routing to the already-active global endpoint must reuse the
/// global Worker. A stable ID is authoritative; the display name is used only
/// for legacy routes that do not yet have a stable endpoint ID.
fn explicit_endpoint_matches_global(
    requested_backend: AudioBackend,
    requested_endpoint_id: Option<&str>,
    requested_device_name: Option<&str>,
    global_backend: AudioBackend,
    global_endpoint_id: &str,
    global_device_name: &str,
) -> bool {
    if requested_backend != global_backend {
        return false;
    }

    match requested_endpoint_id {
        Some(requested_id) => requested_id == global_endpoint_id,
        None => requested_device_name.is_some_and(|name| name == global_device_name),
    }
}

fn endpoint_preferences_key(backend: AudioBackend, device_name: &str) -> String {
    format!("{}::{}", backend.label(), device_name)
}

fn normalize_endpoint_performance_profiles(
    mut profiles: HashMap<String, AudioPerformanceSettings>,
) -> HashMap<String, AudioPerformanceSettings> {
    for (key, profile) in &mut profiles {
        if key.starts_with("ASIO::") {
            *profile = AudioPerformanceSettings::default();
        }
    }
    profiles
}

fn apply_saved_endpoint_output_settings(
    worker: &ManagedAudioWorker,
    settings_by_device: &HashMap<String, Vec<OutputChannelSetting>>,
) {
    let settings_key = endpoint_preferences_key(worker.backend, &worker.device_name);
    if let Some(settings) = settings_by_device.get(&settings_key) {
        if let Err(error) = worker
            .command_tx
            .send(AudioCommand::SetOutputChannelSettings {
                device_name: worker.device_name.clone(),
                settings: settings.clone(),
            })
        {
            warn!(
                "Could not apply saved output-channel settings to {} — {}: {}",
                worker.backend.short_label(),
                worker.device_name,
                error
            );
        }
    }
}

fn spawn_cue_endpoint_worker(
    mut config: WorkerStartupConfig,
    job_supervisor: Arc<AudioWorkerJobSupervisor>,
) -> Result<(SpawnedAudioWorker, AudioPerformanceSettings)> {
    config.performance = AudioPerformanceSettings::for_backend(config.backend, config.performance);
    let requested_performance = config.performance;
    match spawn_audio_worker(config.clone(), job_supervisor.clone()) {
        Ok(spawned) => Ok((spawned, requested_performance)),
        Err(primary_error) if requested_performance != AudioPerformanceSettings::default() => {
            warn!(
                "Endpoint {} — '{}' rejected its saved performance settings {:?}: {:#}. Retrying with Automatic settings for this endpoint.",
                config.backend.short_label(),
                config.selected_device.as_deref().unwrap_or("default output"),
                config.performance,
                primary_error
            );
            let mut fallback_config = config;
            fallback_config.performance = AudioPerformanceSettings::default();
            spawn_audio_worker(fallback_config, job_supervisor)
                .map(|spawned| (spawned, requested_performance))
                .with_context(|| {
                    format!(
                        "Initial performance settings failed ({primary_error:#}) and Automatic settings also failed"
                    )
                })
        }
        Err(error) => Err(error),
    }
}

fn worker_identity_matches_request(
    metadata: &WorkerEngineMetadata,
    backend: AudioBackend,
    endpoint_id: &str,
    device_name: &str,
) -> bool {
    if metadata.backend != backend {
        return false;
    }
    if metadata.current_device_id == endpoint_id {
        return true;
    }

    let fallback_prefix = format!("{}:name:", backend.short_label().to_ascii_lowercase());
    endpoint_id.starts_with(&fallback_prefix) && metadata.current_device == device_name
}

#[derive(Debug, Clone)]
struct EndpointRecoveryProfile {
    backend: AudioBackend,
    endpoint_id: String,
    device_name: String,
    performance: AudioPerformanceSettings,
}

struct EndpointRecoveryResult {
    key: String,
    profile: EndpointRecoveryProfile,
    result: Result<ManagedAudioWorker>,
}

fn send_recovered_cue_states(
    worker_key: &str,
    recovery_cues: &mut HashMap<String, Vec<(Uuid, u64)>>,
    cue_generations: &HashMap<Uuid, u64>,
    position_tx: &Sender<PositionUpdate>,
) {
    let Some(cues) = recovery_cues.remove(worker_key) else {
        return;
    };

    let total_cues = cues.len();
    let mut reset_cues = 0usize;
    for (id, generation) in cues {
        // A newer manual GO owns the Cue. Never overwrite its state with a
        // stale notification from the previous endpoint session.
        if cue_generations.get(&id).is_none() {
            if position_tx
                .try_send(PositionUpdate {
                    id,
                    generation,
                    position_secs: 0.0,
                    state: CueState::Stopped,
                })
                .is_ok()
            {
                reset_cues += 1;
            } else {
                warn!(
                    "Could not publish the recovered Stopped state for Cue {}; endpoint={}",
                    id, worker_key
                );
            }
        }
    }

    info!(
        "Cleared endpoint-recovery Cue errors after Worker recovery: endpoint={}, reset_cues={}, affected_cues={}",
        worker_key, reset_cues, total_cues
    );
}

fn spawn_endpoint_recovery(
    key: String,
    profile: EndpointRecoveryProfile,
    registry_endpoints: Vec<AudioEndpoint>,
    job_supervisor: Arc<AudioWorkerJobSupervisor>,
    recovery_tx: Sender<EndpointRecoveryResult>,
    recovering_endpoints: &mut HashSet<String>,
    shutdown_requested: Arc<AtomicBool>,
) {
    if !recovering_endpoints.insert(key.clone()) {
        return;
    }

    let thread_key = key.clone();
    let thread_profile = profile.clone();
    let thread_shutdown_requested = shutdown_requested.clone();
    let thread_recovery_tx = recovery_tx.clone();
    let spawn = thread::Builder::new()
        .name(format!(
            "tasq-audio-recovery-{}",
            profile.backend.short_label()
        ))
        .spawn(move || {
            if thread_shutdown_requested.load(Ordering::Acquire) {
                return;
            }
            info!(
                "Starting asynchronous endpoint recovery: {} — '{}' [{}]",
                thread_profile.backend.short_label(),
                thread_profile.device_name,
                thread_profile.endpoint_id
            );
            let result = spawn_managed_endpoint_worker(
                thread_profile.backend,
                &thread_profile.endpoint_id,
                &thread_profile.device_name,
                &registry_endpoints,
                thread_profile.performance,
                job_supervisor,
            );
            if thread_shutdown_requested.load(Ordering::Acquire) {
                if let Ok(worker) = result {
                    drop(worker);
                }
                return;
            }
            let _ = thread_recovery_tx.send(EndpointRecoveryResult {
                key: thread_key,
                profile: thread_profile,
                result,
            });
        });

    if let Err(error) = spawn {
        recovering_endpoints.remove(&key);
        let message = format!("Could not start endpoint recovery thread: {error}");
        error!(
            "Endpoint recovery could not be scheduled for {} — '{}': {}",
            profile.backend.short_label(),
            profile.device_name,
            message
        );
        let _ = recovery_tx.send(EndpointRecoveryResult {
            key,
            profile,
            result: Err(anyhow!(message)),
        });
    }
}

/// Start a fresh worker for one exact endpoint. This is used when an endpoint
/// cannot safely rebuild its native stream in-process (notably SDL DirectSound,
/// where closing the old queue can block inside the driver).
fn spawn_managed_endpoint_worker(
    backend: AudioBackend,
    endpoint_id: &str,
    device_name: &str,
    registry_endpoints: &[AudioEndpoint],
    performance: AudioPerformanceSettings,
    job_supervisor: Arc<AudioWorkerJobSupervisor>,
) -> Result<ManagedAudioWorker> {
    let performance = AudioPerformanceSettings::for_backend(backend, performance);
    let config = WorkerStartupConfig {
        protocol_version: WORKER_PROTOCOL_VERSION,
        backend,
        selected_device_id: Some(endpoint_id.to_string()),
        selected_device: Some(device_name.to_string()),
        performance,
        include_full_device_metadata: true,
        registry_endpoints: Some(registry_endpoints.to_vec()),
    };
    let (spawned, requested_performance) = spawn_cue_endpoint_worker(config, job_supervisor)?;
    if !worker_identity_matches_request(&spawned.metadata, backend, endpoint_id, device_name) {
        let actual_backend = spawned.metadata.backend;
        let actual_name = spawned.metadata.current_device.clone();
        let actual_id = spawned.metadata.current_device_id.clone();
        drop(spawned);
        return Err(anyhow!(
            "Requested {} endpoint '{}' [{}], but replacement worker resolved {} '{}' [{}]",
            backend.short_label(),
            device_name,
            endpoint_id,
            actual_backend.short_label(),
            actual_name,
            actual_id
        ));
    }
    Ok(ManagedAudioWorker::from_spawned(
        spawned,
        Some(endpoint_id),
        requested_performance,
    ))
}

/// Stop all Cues owned by a worker and consume their final state updates before
/// replacing the process. The engine emits the exact last position; any stale
/// ownership left after a driver hang is explicitly settled as Stopped.
fn stop_worker_cues_for_restart(
    worker_key: &str,
    workers: &HashMap<String, ManagedAudioWorker>,
    cue_owners: &mut HashMap<Uuid, String>,
    cue_generations: &mut HashMap<Uuid, u64>,
    position_tx: &Sender<PositionUpdate>,
) {
    if let Some(worker) = workers.get(worker_key) {
        let _ = worker.command_tx.send(AudioCommand::StopAll);
    }

    let deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < deadline
        && cue_owners
            .values()
            .any(|owner| owner.as_str() == worker_key)
    {
        drain_worker_positions(workers, cue_owners, cue_generations, position_tx);
        if cue_owners
            .values()
            .any(|owner| owner.as_str() == worker_key)
        {
            thread::sleep(Duration::from_millis(5));
        }
    }

    let stale_cues: Vec<(Uuid, Option<u64>)> = cue_owners
        .iter()
        .filter(|(_, owner)| owner.as_str() == worker_key)
        .map(|(id, _)| (*id, cue_generations.get(id).copied()))
        .collect();
    for (id, generation) in stale_cues {
        cue_owners.remove(&id);
        cue_generations.remove(&id);
        if let Some(generation) = generation {
            let _ = position_tx.try_send(PositionUpdate {
                id,
                generation,
                position_secs: 0.0,
                state: CueState::Stopped,
            });
        }
    }
}

/// Replace an endpoint worker process with the requested performance profile.
/// If the new profile is rejected, try to restore the previous working profile
/// before reporting the failure to the caller.
fn restart_managed_worker_with_performance(
    workers: &mut HashMap<String, ManagedAudioWorker>,
    worker_key: &str,
    registry_endpoints: &[AudioEndpoint],
    requested_performance: AudioPerformanceSettings,
    pending_output_settings: &mut HashMap<String, Vec<OutputChannelSetting>>,
    endpoint_output_channel_settings_by_device: &HashMap<String, Vec<OutputChannelSetting>>,
    job_supervisor: Arc<AudioWorkerJobSupervisor>,
) -> Result<(usize, u32)> {
    let old_worker = workers
        .remove(worker_key)
        .ok_or_else(|| anyhow!("Audio endpoint worker '{}' is not running", worker_key))?;
    let backend = old_worker.backend;
    let requested_performance =
        AudioPerformanceSettings::for_backend(backend, requested_performance);
    let endpoint_id = old_worker.endpoint_id.clone();
    let device_name = old_worker.device_name.clone();
    let previous_performance = old_worker.performance_settings;

    info!(
        "Restarting endpoint worker {} — '{}' to apply performance profile {:?} (previous {:?})",
        backend.short_label(),
        device_name,
        requested_performance,
        previous_performance
    );
    let _ = old_worker.command_tx.try_send(AudioCommand::Shutdown);
    drop(old_worker);

    let replacement = spawn_managed_endpoint_worker(
        backend,
        &endpoint_id,
        &device_name,
        registry_endpoints,
        requested_performance,
        job_supervisor.clone(),
    );

    let (mut worker, applied) = match replacement {
        Ok(worker) => (worker, Ok(())),
        Err(request_error) if previous_performance != requested_performance => {
            warn!(
                "Could not restart {} — '{}' with requested performance profile {:?}: {:#}. Restoring previous profile {:?}.",
                backend.short_label(),
                device_name,
                requested_performance,
                request_error,
                previous_performance
            );
            match spawn_managed_endpoint_worker(
                backend,
                &endpoint_id,
                &device_name,
                registry_endpoints,
                previous_performance,
                job_supervisor.clone(),
            ) {
                Ok(worker) => (
                    worker,
                    Err(anyhow!(
                        "Could not apply performance profile {:?}: {:#}. Previous profile {:?} was restored.",
                        requested_performance,
                        request_error,
                        previous_performance
                    )),
                ),
                Err(restore_error) => {
                    return Err(anyhow!(
                        "Could not apply performance profile {:?}: {:#}; restoring previous profile {:?} also failed: {:#}",
                        requested_performance,
                        request_error,
                        previous_performance,
                        restore_error
                    ));
                }
            }
        }
        Err(error) => return Err(error),
    };

    worker.requested_performance_settings = requested_performance;
    let channels = worker.output_channels;
    let sample_rate = worker.sample_rate;
    let actual_key = endpoint_worker_key(worker.backend, &worker.endpoint_id);
    workers.insert(actual_key.clone(), worker);
    if let Some(worker) = workers.get(&actual_key) {
        apply_pending_output_settings(worker, pending_output_settings);
        apply_saved_endpoint_output_settings(worker, endpoint_output_channel_settings_by_device);
    }

    applied.map(|_| (channels, sample_rate))
}

/// Reconfigure a running worker in-place. The caller may choose a process
/// restart when the backend's native stream teardown is not reliable.
fn reconfigure_managed_worker(
    worker: &mut ManagedAudioWorker,
    performance: AudioPerformanceSettings,
    timeout: Duration,
) -> Result<(usize, u32)> {
    let performance = AudioPerformanceSettings::for_backend(worker.backend, performance);
    let (reply_tx, reply_rx) = bounded(1);
    worker
        .command_tx
        .send(AudioCommand::ReconfigureOutput {
            performance,
            reply: reply_tx,
        })
        .map_err(|error| anyhow!("Could not send performance reconfiguration: {error}"))?;

    let (channels, sample_rate) = reply_rx
        .recv_timeout(timeout)
        .map_err(|error| anyhow!("Worker did not confirm performance reconfiguration: {error}"))?
        .map_err(|error| anyhow!("{error}"))?;

    worker.output_channels = channels.max(1);
    worker.sample_rate = sample_rate;
    worker.performance_settings = performance;
    worker.requested_performance_settings = performance;
    Ok((worker.output_channels, worker.sample_rate))
}

/// Command-router thread owned by the GUI process. It keeps the UI-facing
/// AudioCommand API intact while routing each Cue to the worker that owns its
/// selected endpoint. Each worker owns its native driver and audio stream.
pub(crate) fn run_audio_worker_manager(
    initial_worker: SpawnedAudioWorker,
    command_rx: Receiver<AudioCommand>,
    position_tx: Sender<PositionUpdate>,
    mut registry_endpoints: Vec<AudioEndpoint>,
    shutdown_requested: Arc<AtomicBool>,
    job_supervisor: Arc<AudioWorkerJobSupervisor>,
    event_tx: Sender<AudioWorkerManagerEvent>,
) {
    let initial_requested_performance = initial_worker.metadata.performance_settings;
    let initial =
        ManagedAudioWorker::from_spawned(initial_worker, None, initial_requested_performance);
    let global_key = endpoint_worker_key(initial.backend, &initial.endpoint_id);
    let global_backend = initial.backend;
    let mut global_device_name = initial.device_name.clone();
    let mut global_endpoint_id = initial.endpoint_id.clone();

    info!(
        "Audio worker manager started: global={} — {} [{}], worker_pid={}",
        global_backend.short_label(),
        global_device_name,
        global_endpoint_id,
        initial.pid
    );

    let mut workers = HashMap::new();
    workers.insert(global_key.clone(), initial);
    let mut cue_owners: HashMap<Uuid, String> = HashMap::new();
    let mut cue_generations: HashMap<Uuid, u64> = HashMap::new();
    let mut pending_output_settings: HashMap<String, Vec<OutputChannelSetting>> = HashMap::new();
    let mut endpoint_performance_settings_by_device: HashMap<String, AudioPerformanceSettings> =
        HashMap::new();
    let mut endpoint_output_channel_settings_by_device: HashMap<String, Vec<OutputChannelSetting>> =
        HashMap::new();
    let mut global_key = global_key;
    let mut next_health_check = Instant::now();
    let mut next_output_meter_event = Instant::now() + Duration::from_millis(100);
    let mut next_performance_snapshot = Instant::now() + Duration::from_secs(1);
    let (recovery_tx, recovery_rx) = unbounded::<EndpointRecoveryResult>();
    let mut recovery_profiles: HashMap<String, EndpointRecoveryProfile> = HashMap::new();
    let mut recovering_endpoints: HashSet<String> = HashSet::new();
    let mut recovery_retry_after: HashMap<String, Instant> = HashMap::new();
    let mut recovery_attempts: HashMap<String, u8> = HashMap::new();
    let mut recovery_cues: HashMap<String, Vec<(Uuid, u64)>> = HashMap::new();
    let mut active_launch_transaction: Option<Uuid> = None;
    // Record the process identity as well as the endpoint key. If a Worker is
    // replaced during a launch transaction, the replacement must receive its
    // own Begin marker rather than inheriting the old process's state.
    let mut launch_transaction_workers: HashMap<String, u32> = HashMap::new();

    loop {
        if shutdown_requested.load(Ordering::Acquire) {
            break;
        }
        drain_worker_positions(
            &workers,
            &mut cue_owners,
            &mut cue_generations,
            &position_tx,
        );
        drain_worker_output_meters(&mut workers, &event_tx, &mut next_output_meter_event);
        let performance_now = Instant::now();
        if performance_now >= next_performance_snapshot {
            let mut worker_pids = Vec::with_capacity(workers.len());
            let mut heartbeat_max_age_ms: Option<u64> = None;
            for worker in workers.values() {
                if worker.pid > 0 {
                    worker_pids.push(worker.pid);
                }
                let heartbeat_age_ms = {
                    let health = worker.health.lock();
                    performance_now
                        .saturating_duration_since(health.last_heartbeat)
                        .as_millis()
                        .min(u64::MAX as u128) as u64
                };
                heartbeat_max_age_ms = Some(
                    heartbeat_max_age_ms
                        .map_or(heartbeat_age_ms, |current| current.max(heartbeat_age_ms)),
                );
            }
            worker_pids.sort_unstable();
            let _ = event_tx.send(AudioWorkerManagerEvent::PerformanceSnapshot {
                worker_pids,
                heartbeat_max_age_ms,
            });
            next_performance_snapshot = performance_now + Duration::from_secs(1);
        }
        if Instant::now() >= next_health_check {
            let health_failures = monitor_worker_health(&mut workers);
            next_health_check = Instant::now() + WORKER_HEALTH_CHECK_INTERVAL;

            for (key, issue) in health_failures {
                let Some(mut worker) = workers.remove(&key) else {
                    continue;
                };
                let is_global = key == global_key;
                let profile = EndpointRecoveryProfile {
                    backend: worker.backend,
                    endpoint_id: worker.endpoint_id.clone(),
                    device_name: worker.device_name.clone(),
                    performance: worker.requested_performance_settings,
                };
                let affected: Vec<(Uuid, u64)> = cue_owners
                    .iter()
                    .filter(|(_, owner)| owner.as_str() == key.as_str())
                    .filter_map(|(cue_id, _)| {
                        cue_generations
                            .get(cue_id)
                            .copied()
                            .map(|generation| (*cue_id, generation))
                    })
                    .collect();

                for (cue_id, generation) in &affected {
                    cue_owners.remove(cue_id);
                    cue_generations.remove(cue_id);
                    if let Err(error) = position_tx.try_send(PositionUpdate {
                        id: *cue_id,
                        generation: *generation,
                        position_secs: 0.0,
                        state: CueState::Error(format!(
                            "Audio worker {} — '{}' failed ({}). Playback stopped; automatic recovery is being attempted.",
                            profile.backend.short_label(),
                            profile.device_name,
                            issue.description()
                        )),
                    }) {
                        warn!(
                            "Could not publish the failed Worker state for Cue {} (endpoint={}): {}",
                            cue_id, key, error
                        );
                    }
                }
                if !affected.is_empty() {
                    recovery_cues.insert(key.clone(), affected.clone());
                }

                recovery_profiles.insert(key.clone(), profile.clone());
                recovery_attempts.insert(key.clone(), 0);
                recovery_retry_after.remove(&key);
                let first_attempt = schedule_next_recovery_attempt(
                    &key,
                    &recovery_attempts,
                    &mut recovery_retry_after,
                );
                match first_attempt {
                    RecoveryRetryPlan::Scheduled {
                        next_attempt,
                        delay,
                    } => warn!(
                        "Isolated unhealthy audio worker: endpoint={}, backend={}, device='{}', pid={}, issue={}, affected_cues={}; recovery cycle started, attempt {}/{} scheduled in {}s (endpoint availability will be checked before creating a Worker)",
                        key,
                        profile.backend.short_label(),
                        profile.device_name,
                        worker.pid,
                        issue.description(),
                        affected.len(),
                        next_attempt,
                        MAX_AUTOMATIC_RECOVERY_ATTEMPTS,
                        delay.as_secs()
                    ),
                    RecoveryRetryPlan::Exhausted { attempts } => error!(
                        "Could not start automatic recovery cycle for {} — '{}': attempt budget already exhausted ({}/{})",
                        key,
                        profile.device_name,
                        attempts,
                        MAX_AUTOMATIC_RECOVERY_ATTEMPTS
                    ),
                }

                let _ = event_tx.send(AudioWorkerManagerEvent::EndpointRecoveryFailed {
                    backend: profile.backend,
                    endpoint_id: profile.endpoint_id.clone(),
                    device_name: profile.device_name.clone(),
                    is_global,
                    error: format!(
                        "Audio Worker health failure: {}. Affected Cues were stopped.",
                        issue.description()
                    ),
                    retry_scheduled: true,
                });

                let worker_health = worker.health.clone();
                if let Err(error) = worker._process.request_force_termination(&worker_health) {
                    warn!(
                        "Could not force-terminate unhealthy audio worker {}: {:#}",
                        key, error
                    );
                }
                let cleanup_key = key.clone();
                let cleanup_name = format!(
                    "tasq-audio-unhealthy-cleanup-{}",
                    profile.backend.short_label()
                );
                if let Err(error) = thread::Builder::new()
                    .name(cleanup_name)
                    .spawn(move || drop(worker))
                {
                    warn!(
                        "Could not start background cleanup for unhealthy audio worker {}: {}; process cleanup will be handled by process teardown",
                        cleanup_key, error
                    );
                }
            }
        }

        while let Ok(recovery) = recovery_rx.try_recv() {
            recovering_endpoints.remove(&recovery.key);
            match recovery.result {
                Ok(worker) => {
                    let actual_key = endpoint_worker_key(worker.backend, &worker.endpoint_id);
                    let metadata = worker.metadata.clone();
                    let is_global = actual_key == global_key;
                    info!(
                        "Endpoint recovery succeeded: {} — '{}' [{}], pid={}, channels={}, sample_rate={} Hz",
                        worker.backend.short_label(),
                        worker.device_name,
                        worker.endpoint_id,
                        worker.pid,
                        worker.output_channels,
                        worker.sample_rate
                    );
                    workers.insert(actual_key.clone(), worker);
                    if let Some(worker) = workers.get(&actual_key) {
                        apply_pending_output_settings(worker, &mut pending_output_settings);
                        apply_saved_endpoint_output_settings(
                            worker,
                            &endpoint_output_channel_settings_by_device,
                        );
                    }
                    recovery_profiles.remove(&actual_key);
                    recovery_retry_after.remove(&actual_key);
                    recovery_attempts.remove(&actual_key);
                    send_recovered_cue_states(
                        &actual_key,
                        &mut recovery_cues,
                        &cue_generations,
                        &position_tx,
                    );
                    if is_global {
                        if let Some(worker) = workers.get(&actual_key) {
                            global_device_name = worker.device_name.clone();
                            global_endpoint_id = worker.endpoint_id.clone();
                        }
                    }
                    let _ = event_tx.send(AudioWorkerManagerEvent::EndpointRecovered {
                        metadata,
                        is_global,
                    });
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    let is_global = recovery.key == global_key;
                    let attempts = recovery_attempts
                        .get(&recovery.key)
                        .copied()
                        .unwrap_or(1)
                        .max(1);
                    let retry = schedule_next_recovery_attempt(
                        &recovery.key,
                        &recovery_attempts,
                        &mut recovery_retry_after,
                    );
                    let (retry_scheduled, recovery_message) = match retry {
                        RecoveryRetryPlan::Scheduled {
                            next_attempt,
                            delay,
                        } => {
                            warn!(
                                "Endpoint recovery attempt {}/{} failed for {} — '{}' [{}]: {}; next recovery check {}/{} is scheduled in {}s (a Worker will only be created if the endpoint is available)",
                                attempts,
                                MAX_AUTOMATIC_RECOVERY_ATTEMPTS,
                                recovery.profile.backend.short_label(),
                                recovery.profile.device_name,
                                recovery.profile.endpoint_id,
                                message,
                                next_attempt,
                                MAX_AUTOMATIC_RECOVERY_ATTEMPTS,
                                delay.as_secs()
                            );
                            (
                                true,
                                format!(
                                    "{message}. Recovery check {next_attempt}/{} is scheduled in {}s; TaSQ will only create a Worker if the endpoint is available.",
                                    MAX_AUTOMATIC_RECOVERY_ATTEMPTS,
                                    delay.as_secs()
                                ),
                            )
                        }
                        RecoveryRetryPlan::Exhausted { attempts } => {
                            error!(
                                "Automatic endpoint recovery exhausted after {}/{} attempts: {} — '{}' [{}]: {}. Waiting for a fresh device reconnect or explicit user action.",
                                attempts,
                                MAX_AUTOMATIC_RECOVERY_ATTEMPTS,
                                recovery.profile.backend.short_label(),
                                recovery.profile.device_name,
                                recovery.profile.endpoint_id,
                                message
                            );
                            (
                                false,
                                format!(
                                    "{message}. Automatic recovery stopped after {attempts} attempts; reconnect the device or press GO to start a new recovery cycle."
                                ),
                            )
                        }
                    };
                    let _ = event_tx.send(AudioWorkerManagerEvent::EndpointRecoveryFailed {
                        backend: recovery.profile.backend,
                        endpoint_id: recovery.profile.endpoint_id,
                        device_name: recovery.profile.device_name,
                        is_global,
                        error: recovery_message,
                        retry_scheduled,
                    });
                }
            }
        }

        // Each due timer is one bounded recovery check. Checks continue on
        // schedule even while the device is absent, but no Worker process is
        // created until the registry confirms that the exact endpoint is back.
        let retry_keys: Vec<String> = recovery_retry_after
            .iter()
            .filter_map(|(key, retry_at)| (*retry_at <= Instant::now()).then_some(key.clone()))
            .collect();
        for key in retry_keys {
            recovery_retry_after.remove(&key);
            if workers.contains_key(&key) || recovering_endpoints.contains(&key) {
                continue;
            }
            let Some(profile) = recovery_profiles.get(&key).cloned() else {
                continue;
            };
            let is_global = key == global_key;
            let Some(attempt) = mark_recovery_attempt_started(&key, &mut recovery_attempts) else {
                error!(
                    "Automatic endpoint recovery timer reached after the attempt budget was exhausted: {} — '{}' (maximum {} attempts)",
                    profile.backend.short_label(),
                    profile.device_name,
                    MAX_AUTOMATIC_RECOVERY_ATTEMPTS
                );
                let _ = event_tx.send(AudioWorkerManagerEvent::EndpointRecoveryFailed {
                    backend: profile.backend,
                    endpoint_id: profile.endpoint_id,
                    device_name: profile.device_name,
                    is_global,
                    error: format!(
                        "Automatic recovery stopped after {} attempts. Reconnect the device or press GO to start a new recovery cycle.",
                        MAX_AUTOMATIC_RECOVERY_ATTEMPTS
                    ),
                    retry_scheduled: false,
                });
                continue;
            };

            let endpoint_available = registry_endpoints.iter().any(|endpoint| {
                endpoint.backend == profile.backend
                    && endpoint.id == profile.endpoint_id
                    && endpoint.availability.is_available()
            });

            if endpoint_available {
                info!(
                    "Starting automatic endpoint recovery attempt {}/{}: {} — '{}' [{}]",
                    attempt,
                    MAX_AUTOMATIC_RECOVERY_ATTEMPTS,
                    profile.backend.short_label(),
                    profile.device_name,
                    profile.endpoint_id
                );
                spawn_endpoint_recovery(
                    key.clone(),
                    profile,
                    registry_endpoints.clone(),
                    job_supervisor.clone(),
                    recovery_tx.clone(),
                    &mut recovering_endpoints,
                    shutdown_requested.clone(),
                );
            } else {
                match schedule_next_recovery_attempt(
                    &key,
                    &recovery_attempts,
                    &mut recovery_retry_after,
                ) {
                    RecoveryRetryPlan::Scheduled {
                        next_attempt,
                        delay,
                    } => {
                        warn!(
                            "Automatic endpoint recovery attempt {}/{} skipped for {} — '{}' [{}]: endpoint is still unavailable; no Worker was created. Next availability check {}/{} in {}s",
                            attempt,
                            MAX_AUTOMATIC_RECOVERY_ATTEMPTS,
                            profile.backend.short_label(),
                            profile.device_name,
                            profile.endpoint_id,
                            next_attempt,
                            MAX_AUTOMATIC_RECOVERY_ATTEMPTS,
                            delay.as_secs()
                        );
                        let _ = event_tx.send(AudioWorkerManagerEvent::EndpointRecoveryFailed {
                            backend: profile.backend,
                            endpoint_id: profile.endpoint_id,
                            device_name: profile.device_name,
                            is_global,
                            error: format!(
                                "Recovery check {attempt}/{} found the endpoint unavailable. The next check ({next_attempt}/{}) is scheduled in {}s; no Worker was created.",
                                MAX_AUTOMATIC_RECOVERY_ATTEMPTS,
                                MAX_AUTOMATIC_RECOVERY_ATTEMPTS,
                                delay.as_secs()
                            ),
                            retry_scheduled: true,
                        });
                    }
                    RecoveryRetryPlan::Exhausted { attempts } => {
                        error!(
                            "Automatic endpoint recovery exhausted after {}/{} checks for {} — '{}' [{}]: device did not return; no Worker was created. Waiting for a fresh reconnect or explicit user action.",
                            attempts,
                            MAX_AUTOMATIC_RECOVERY_ATTEMPTS,
                            profile.backend.short_label(),
                            profile.device_name,
                            profile.endpoint_id
                        );
                        let _ = event_tx.send(AudioWorkerManagerEvent::EndpointRecoveryFailed {
                            backend: profile.backend,
                            endpoint_id: profile.endpoint_id,
                            device_name: profile.device_name,
                            is_global,
                            error: format!(
                                "The endpoint remained unavailable. Automatic recovery stopped after {attempts} checks; reconnect the device or press GO to start a new recovery cycle."
                            ),
                            retry_scheduled: false,
                        });
                    }
                }
            }
        }

        match command_rx.recv_timeout(Duration::from_millis(5)) {
            Ok(AudioCommand::BeginLaunchTransaction { transaction_id }) => {
                if let Some(previous_id) = active_launch_transaction.replace(transaction_id) {
                    warn!(
                        "New GUI launch transaction {} arrived before {} ended; closing previous Worker transactions",
                        transaction_id,
                        previous_id
                    );
                    for (worker_key, worker_pid) in launch_transaction_workers.drain() {
                        if let Some(worker) = workers
                            .get(&worker_key)
                            .filter(|worker| worker.pid == worker_pid)
                        {
                            let _ = worker.command_tx.send(AudioCommand::EndLaunchTransaction {
                                transaction_id: previous_id,
                            });
                        }
                    }
                }
                launch_transaction_workers.clear();
                debug!(
                    "Audio Worker Manager began launch transaction {}",
                    transaction_id
                );
            }
            Ok(AudioCommand::EndLaunchTransaction { transaction_id }) => {
                match active_launch_transaction {
                    Some(active_id) if active_id == transaction_id => {
                        let worker_keys: Vec<(String, u32)> =
                            launch_transaction_workers.drain().collect();
                        for (worker_key, worker_pid) in &worker_keys {
                            if let Some(worker) = workers
                                .get(worker_key)
                                .filter(|worker| worker.pid == *worker_pid)
                            {
                                if let Err(error) = worker
                                    .command_tx
                                    .send(AudioCommand::EndLaunchTransaction { transaction_id })
                                {
                                    error!(
                                        "Could not close launch transaction {} on worker {} (pid={}): {}",
                                        transaction_id,
                                        worker_key,
                                        worker_pid,
                                        error
                                    );
                                }
                            }
                        }
                        active_launch_transaction = None;
                        info!(
                            "Audio launch transaction completed: transaction_id={}, endpoint_workers={}",
                            transaction_id,
                            worker_keys.len()
                        );
                    }
                    Some(active_id) => warn!(
                        "Ignoring mismatched GUI launch transaction end: requested={}, active={}",
                        transaction_id, active_id
                    ),
                    None => warn!(
                        "Received GUI launch transaction end without an active transaction: {}",
                        transaction_id
                    ),
                }
            }
            Ok(AudioCommand::PlayBatch { .. }) => {
                error!("PlayBatch reached the GUI Worker Manager before endpoint routing; this command is only valid inside an endpoint Worker");
            }
            Ok(AudioCommand::Play {
                id,
                generation,
                file,
                output_device,
                output_backend,
                output_device_id,
                start_secs,
                end_secs,
                duration_secs,
                volume,
                volume_automation_enabled,
                volume_automation,
                time_pitch,
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
            }) => {
                let (target_backend, target_id, target_name, target_key) = if output_device
                    .is_some()
                    || output_device_id.is_some()
                {
                    let backend = output_backend.unwrap_or(global_backend);
                    if explicit_endpoint_matches_global(
                        backend,
                        output_device_id.as_deref(),
                        output_device.as_deref(),
                        global_backend,
                        &global_endpoint_id,
                        &global_device_name,
                    ) {
                        // Global Default and an explicit route to that exact
                        // same endpoint are one Worker, not two startup paths.
                        (
                            global_backend,
                            global_endpoint_id.clone(),
                            global_device_name.clone(),
                            global_key.clone(),
                        )
                    } else {
                        let registered_by_id = output_device_id
                            .as_deref()
                            .and_then(|id| registry_endpoints.iter().find(|ep| ep.id == id))
                            .filter(|endpoint| endpoint.backend == backend);
                        let requested_name = output_device.as_deref().or_else(|| {
                            registered_by_id.map(|endpoint| endpoint.device_name.as_str())
                        });
                        let registered = registered_by_id.or_else(|| {
                            requested_name.and_then(|device_name| {
                                registry_endpoints.iter().find(|endpoint| {
                                    endpoint.backend == backend
                                        && endpoint.device_name == device_name
                                        && endpoint.availability.is_available()
                                })
                            })
                        });
                        let resolved_name = registered
                            .map(|endpoint| endpoint.device_name.clone())
                            .or_else(|| output_device.clone())
                            .unwrap_or_else(|| "Unavailable endpoint".to_string());
                        let endpoint_id = registered
                            .map(|endpoint| endpoint.id.clone())
                            .or(output_device_id)
                            .unwrap_or_else(|| {
                                format!("{}:name:{}", backend.short_label(), resolved_name)
                            });
                        let key = endpoint_worker_key(backend, &endpoint_id);
                        (backend, endpoint_id, resolved_name, key)
                    }
                } else if let Some(global) = workers.get(&global_key) {
                    (
                        global.backend,
                        global.endpoint_id.clone(),
                        global.device_name.clone(),
                        global_key.clone(),
                    )
                } else if let Some(global_endpoint) = registry_endpoints.iter().find(|endpoint| {
                    endpoint.backend == global_backend
                        && endpoint.id == global_endpoint_id
                        && endpoint.availability.is_available()
                }) {
                    // The global worker may have failed automatic recovery. A
                    // manual GO may recreate it on demand, but never resumes
                    // an interrupted Cue without that explicit new GO.
                    (
                        global_backend,
                        global_endpoint.id.clone(),
                        global_endpoint.device_name.clone(),
                        global_key.clone(),
                    )
                } else {
                    error!("Global audio worker is missing; cannot route Cue {}", id);
                    let _ = position_tx.try_send(PositionUpdate {
                        id,
                        generation,
                        position_secs: 0.0,
                        state: CueState::Error("Global audio worker is unavailable.".to_string()),
                    });
                    continue;
                };

                if recovering_endpoints.contains(&target_key) {
                    let pending = recovery_cues.entry(target_key.clone()).or_default();
                    if !pending.iter().any(|(cue_id, cue_generation)| {
                        *cue_id == id && *cue_generation == generation
                    }) {
                        pending.push((id, generation));
                    }
                    let _ = position_tx.try_send(PositionUpdate {
                        id,
                        generation,
                        position_secs: 0.0,
                        state: CueState::Error(format!(
                            "{} — '{}' is reconnecting. Wait for recovery to finish, then press GO again.",
                            target_backend.short_label(),
                            target_name
                        )),
                    });
                    continue;
                }

                let endpoint_settings_key = endpoint_preferences_key(target_backend, &target_name);
                let requested_performance = AudioPerformanceSettings::for_backend(
                    target_backend,
                    endpoint_performance_settings_by_device
                        .get(&endpoint_settings_key)
                        .copied()
                        .unwrap_or_default(),
                );
                if let Some((current_performance, last_requested_performance)) =
                    workers.get(&target_key).map(|worker| {
                        (
                            worker.performance_settings,
                            worker.requested_performance_settings,
                        )
                    })
                {
                    if current_performance == requested_performance {
                        if let Some(worker) = workers.get_mut(&target_key) {
                            worker.requested_performance_settings = requested_performance;
                        }
                    } else if last_requested_performance != requested_performance {
                        let other_cues_active = cue_owners.iter().any(|(owner_id, owner_key)| {
                            owner_key == &target_key && *owner_id != id
                        });
                        if other_cues_active {
                            warn!(
                                "Endpoint {} — '{}' has a newer performance profile {:?}, but it is kept at {:?} because other Cues are still active on this worker",
                                target_backend.short_label(),
                                target_name,
                                requested_performance,
                                current_performance
                            );
                        } else {
                            if target_backend == AudioBackend::DirectSound {
                                stop_worker_cues_for_restart(
                                    &target_key,
                                    &workers,
                                    &mut cue_owners,
                                    &mut cue_generations,
                                    &position_tx,
                                );
                            }

                            let result = if target_backend == AudioBackend::DirectSound {
                                restart_managed_worker_with_performance(
                                    &mut workers,
                                    &target_key,
                                    &registry_endpoints,
                                    requested_performance,
                                    &mut pending_output_settings,
                                    &endpoint_output_channel_settings_by_device,
                                    job_supervisor.clone(),
                                )
                            } else {
                                let in_place = workers
                                    .get_mut(&target_key)
                                    .ok_or_else(|| {
                                        anyhow!("Endpoint worker '{}' disappeared", target_key)
                                    })
                                    .and_then(|worker| {
                                        reconfigure_managed_worker(
                                            worker,
                                            requested_performance,
                                            Duration::from_secs(8),
                                        )
                                    });
                                if in_place.is_err() {
                                    stop_worker_cues_for_restart(
                                        &target_key,
                                        &workers,
                                        &mut cue_owners,
                                        &mut cue_generations,
                                        &position_tx,
                                    );
                                    restart_managed_worker_with_performance(
                                        &mut workers,
                                        &target_key,
                                        &registry_endpoints,
                                        requested_performance,
                                        &mut pending_output_settings,
                                        &endpoint_output_channel_settings_by_device,
                                        job_supervisor.clone(),
                                    )
                                } else {
                                    in_place
                                }
                            };

                            if let Err(error) = result {
                                error!(
                                    "Could not apply the saved performance profile to {} — '{}': {:#}",
                                    target_backend.short_label(),
                                    target_name,
                                    error
                                );
                                let _ = position_tx.try_send(PositionUpdate {
                                    id,
                                    generation,
                                    position_secs: 0.0,
                                    state: CueState::Error(format!(
                                        "Could not configure {} output '{}': {error:#}",
                                        target_backend.short_label(),
                                        target_name
                                    )),
                                });
                                continue;
                            }
                        }
                    }
                }

                if !workers.contains_key(&target_key) {
                    info!(
                        "Starting endpoint worker on demand for Cue {}: {} — {} [{}]",
                        id,
                        target_backend.short_label(),
                        target_name,
                        target_id
                    );
                    let config = WorkerStartupConfig {
                        protocol_version: WORKER_PROTOCOL_VERSION,
                        backend: target_backend,
                        selected_device_id: Some(target_id.clone()),
                        selected_device: Some(target_name.clone()),
                        performance: requested_performance,
                        include_full_device_metadata: false,
                        registry_endpoints: Some(registry_endpoints.clone()),
                    };
                    match spawn_cue_endpoint_worker(config, job_supervisor.clone()) {
                        Ok((spawned, requested_profile)) => {
                            if !worker_identity_matches_request(
                                &spawned.metadata,
                                target_backend,
                                &target_id,
                                &target_name,
                            ) {
                                let actual_backend = spawned.metadata.backend;
                                let actual_name = spawned.metadata.current_device.clone();
                                let actual_id = spawned.metadata.current_device_id.clone();
                                drop(spawned);
                                let message = format!(
                                    "Requested {} endpoint '{}' [{}], but the worker resolved {} '{}' [{}]. Refusing to route the Cue to a different endpoint.",
                                    target_backend.short_label(),
                                    target_name,
                                    target_id,
                                    actual_backend.short_label(),
                                    actual_name,
                                    actual_id
                                );
                                error!("{}", message);
                                let _ = position_tx.try_send(PositionUpdate {
                                    id,
                                    generation,
                                    position_secs: 0.0,
                                    state: CueState::Error(message),
                                });
                                continue;
                            }
                            let worker = ManagedAudioWorker::from_spawned(
                                spawned,
                                Some(&target_id),
                                requested_profile,
                            );
                            let actual_key =
                                endpoint_worker_key(worker.backend, &worker.endpoint_id);
                            let recovery_metadata = worker.metadata.clone();
                            let recovered_device_name = worker.device_name.clone();
                            let recovered_endpoint_id = worker.endpoint_id.clone();
                            if worker.performance_settings != requested_performance {
                                warn!(
                                    "Endpoint {} — '{}' is running with fallback performance settings {:?}; requested profile {:?} remains saved for the next attempt",
                                    worker.backend.short_label(),
                                    worker.device_name,
                                    worker.performance_settings,
                                    requested_performance
                                );
                            }
                            workers.insert(actual_key.clone(), worker);
                            let had_recovery_profile =
                                recovery_profiles.remove(&actual_key).is_some();
                            recovery_retry_after.remove(&actual_key);
                            recovery_attempts.remove(&actual_key);
                            let is_global = actual_key == global_key;
                            if is_global {
                                global_device_name = recovered_device_name;
                                global_endpoint_id = recovered_endpoint_id;
                            }
                            if had_recovery_profile || is_global {
                                let _ = event_tx.send(AudioWorkerManagerEvent::EndpointRecovered {
                                    metadata: recovery_metadata,
                                    is_global,
                                });
                            }
                            send_recovered_cue_states(
                                &actual_key,
                                &mut recovery_cues,
                                &cue_generations,
                                &position_tx,
                            );
                            if let Some(worker) = workers.get(&actual_key) {
                                apply_pending_output_settings(worker, &mut pending_output_settings);
                                apply_saved_endpoint_output_settings(
                                    worker,
                                    &endpoint_output_channel_settings_by_device,
                                );
                            }
                        }
                        Err(error) => {
                            // A failed restart must not leave an older stream
                            // playing invisibly under the same Cue ID.
                            if let Some(previous_key) = cue_owners.remove(&id) {
                                if let Some(previous) = workers.get(&previous_key) {
                                    let _ = previous.command_tx.send(AudioCommand::Stop(id));
                                }
                            }
                            cue_generations.remove(&id);
                            error!(
                                "Could not start endpoint worker for Cue {} on {} — {} [{}]: {:#}",
                                id,
                                target_backend.short_label(),
                                target_name,
                                target_id,
                                error
                            );
                            let _ = position_tx.try_send(PositionUpdate {
                                id,
                                generation,
                                position_secs: 0.0,
                                state: CueState::Error(format!(
                                    "Could not start {} output '{}': {error:#}",
                                    target_backend.short_label(),
                                    target_name
                                )),
                            });
                            continue;
                        }
                    }
                }

                let actual_key = target_key;

                if let Some(previous_key) = cue_owners.get(&id).cloned() {
                    if previous_key != actual_key {
                        if let Some(previous) = workers.get(&previous_key) {
                            let _ = previous.command_tx.send(AudioCommand::Stop(id));
                        }
                    }
                }

                let command = AudioCommand::Play {
                    id,
                    generation,
                    file,
                    // A worker owns exactly one endpoint; route to its default stream.
                    output_device: None,
                    output_backend: None,
                    output_device_id: None,
                    start_secs,
                    end_secs,
                    duration_secs,
                    volume,
                    volume_automation_enabled,
                    volume_automation,
                    time_pitch,
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
                };

                if let Some(worker) = workers.get(&actual_key) {
                    let launch_marker_ready = if let Some(transaction_id) =
                        active_launch_transaction
                    {
                        if launch_transaction_workers.get(&actual_key).copied() == Some(worker.pid)
                        {
                            true
                        } else {
                            match worker
                                .command_tx
                                .send(AudioCommand::BeginLaunchTransaction { transaction_id })
                            {
                                Ok(()) => {
                                    launch_transaction_workers
                                        .insert(actual_key.clone(), worker.pid);
                                    true
                                }
                                Err(error) => {
                                    error!(
                                        "Could not begin launch transaction {} on worker {}: {}",
                                        transaction_id, actual_key, error
                                    );
                                    false
                                }
                            }
                        }
                    } else {
                        true
                    };

                    if !launch_marker_ready {
                        cue_owners.remove(&id);
                        cue_generations.remove(&id);
                        let _ = position_tx.try_send(PositionUpdate {
                            id,
                            generation,
                            position_secs: 0.0,
                            state: CueState::Error(
                                "Could not prepare the audio launch transaction on the selected Worker.".to_string(),
                            ),
                        });
                        continue;
                    }

                    match worker.command_tx.send(command) {
                        Ok(()) => {
                            cue_owners.insert(id, actual_key.clone());
                            cue_generations.insert(id, generation);
                            info!(
                                "Routed Cue {} to worker {} ({} — {}, pid={})",
                                id,
                                actual_key,
                                worker.backend.short_label(),
                                worker.device_name,
                                worker.pid
                            );
                        }
                        Err(error) => {
                            cue_owners.remove(&id);
                            cue_generations.remove(&id);
                            let _ = position_tx.try_send(PositionUpdate {
                                id,
                                generation,
                                position_secs: 0.0,
                                state: CueState::Error(format!(
                                    "Could not send playback command to audio worker: {error}"
                                )),
                            });
                            error!(
                                "Could not send Cue {} to endpoint worker {}: {}",
                                id, actual_key, error
                            );
                        }
                    }
                } else {
                    cue_owners.remove(&id);
                    cue_generations.remove(&id);
                    let _ = position_tx.try_send(PositionUpdate {
                        id,
                        generation,
                        position_secs: 0.0,
                        state: CueState::Error("Selected audio worker is unavailable.".to_string()),
                    });
                }
            }
            Ok(AudioCommand::Stop(id)) => {
                let key = cue_owners
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| global_key.clone());
                // Keep ownership until the worker's Stopped update is forwarded;
                // the UI relies on that event to settle Cue state and position.
                send_to_worker(&workers, &key, AudioCommand::Stop(id));
            }
            Ok(AudioCommand::StopTimed {
                id,
                request_id,
                requested_at_unix_micros,
            }) => {
                let manager_elapsed_us = unix_micros_now().saturating_sub(requested_at_unix_micros);
                let key = cue_owners
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| global_key.clone());
                info!(
                    "STOP_LATENCY: request_id={}, cue_id={}, stage=worker_manager_received, request_to_manager_us={}, endpoint='{}'",
                    request_id, id, manager_elapsed_us, key
                );
                send_to_worker(
                    &workers,
                    &key,
                    AudioCommand::StopTimed {
                        id,
                        request_id,
                        requested_at_unix_micros,
                    },
                );
            }
            Ok(AudioCommand::StopAll) => {
                for worker in workers.values() {
                    let _ = worker.command_tx.send(AudioCommand::StopAll);
                }
                // Individual Stopped events remove ownership as they arrive.
            }
            Ok(AudioCommand::StopAllTimed {
                request_id,
                requested_at_unix_micros,
            }) => {
                let manager_elapsed_us = unix_micros_now().saturating_sub(requested_at_unix_micros);
                info!(
                    "STOP_LATENCY: request_id={}, scope=all, stage=worker_manager_received, request_to_manager_us={}, endpoint_workers={}",
                    request_id, manager_elapsed_us, workers.len()
                );
                for worker in workers.values() {
                    let _ = worker.command_tx.send(AudioCommand::StopAllTimed {
                        request_id,
                        requested_at_unix_micros,
                    });
                }
            }
            Ok(AudioCommand::Pause(id)) => {
                let key = cue_owners
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| global_key.clone());
                send_to_worker(&workers, &key, AudioCommand::Pause(id));
            }
            Ok(AudioCommand::Seek { id, position_secs }) => {
                let key = cue_owners
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| global_key.clone());
                send_to_worker(&workers, &key, AudioCommand::Seek { id, position_secs });
            }
            Ok(AudioCommand::PauseAll) => {
                for worker in workers.values() {
                    let _ = worker.command_tx.send(AudioCommand::PauseAll);
                }
            }
            Ok(AudioCommand::Resume(id)) => {
                let key = cue_owners
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| global_key.clone());
                send_to_worker(&workers, &key, AudioCommand::Resume(id));
            }
            Ok(AudioCommand::ResumeAll) => {
                for worker in workers.values() {
                    let _ = worker.command_tx.send(AudioCommand::ResumeAll);
                }
            }
            Ok(AudioCommand::SetVolume(id, volume)) => {
                let key = cue_owners
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| global_key.clone());
                send_to_worker(&workers, &key, AudioCommand::SetVolume(id, volume));
            }
            Ok(AudioCommand::SetVolumeAutomation {
                id,
                enabled,
                points,
            }) => {
                let key = cue_owners
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| global_key.clone());
                send_to_worker(
                    &workers,
                    &key,
                    AudioCommand::SetVolumeAutomation {
                        id,
                        enabled,
                        points,
                    },
                );
            }
            Ok(AudioCommand::SetTimePitch { id, settings }) => {
                let key = cue_owners
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| global_key.clone());
                send_to_worker(&workers, &key, AudioCommand::SetTimePitch { id, settings });
            }
            Ok(AudioCommand::Fade {
                id,
                target_volume,
                duration_secs,
                stop_on_finish,
                curve,
                custom_fade_points,
            }) => {
                let key = cue_owners
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| global_key.clone());
                send_to_worker(
                    &workers,
                    &key,
                    AudioCommand::Fade {
                        id,
                        target_volume,
                        duration_secs,
                        stop_on_finish,
                        curve,
                        custom_fade_points,
                    },
                );
            }
            Ok(AudioCommand::SetOutputDevice {
                device_name,
                endpoint_id,
                performance: requested_performance,
                reply,
            }) => {
                let requested_performance =
                    AudioPerformanceSettings::for_backend(global_backend, requested_performance);
                let registered_by_id = endpoint_id
                    .as_deref()
                    .and_then(|id| registry_endpoints.iter().find(|endpoint| endpoint.id == id))
                    .filter(|endpoint| {
                        endpoint.backend == global_backend && endpoint.availability.is_available()
                    });
                let registered = registered_by_id.or_else(|| {
                    registry_endpoints.iter().find(|endpoint| {
                        endpoint.backend == global_backend
                            && endpoint.device_name == device_name
                            && endpoint.availability.is_available()
                    })
                });
                let endpoint_id = registered
                    .map(|endpoint| endpoint.id.clone())
                    .or(endpoint_id)
                    .unwrap_or_else(|| {
                        format!("{}:name:{}", global_backend.short_label(), device_name)
                    });
                let requested_key = endpoint_worker_key(global_backend, &endpoint_id);

                let existing_performance = workers
                    .get(&requested_key)
                    .map(|worker| worker.performance_settings);
                if requested_key == global_key
                    && existing_performance.is_none()
                    && !recovery_profiles.contains_key(&requested_key)
                {
                    recovery_profiles.insert(
                        requested_key.clone(),
                        EndpointRecoveryProfile {
                            backend: global_backend,
                            endpoint_id: endpoint_id.clone(),
                            device_name: device_name.clone(),
                            performance: requested_performance,
                        },
                    );
                }
                let recovery_notification_needed = recovery_profiles.contains_key(&requested_key);
                let result = if recovering_endpoints.contains(&requested_key) {
                    Err(anyhow!(
                        "{} — '{}' is still reconnecting; wait for recovery to finish",
                        global_backend.short_label(),
                        device_name
                    ))
                } else if let Some(existing_performance) = existing_performance {
                    if existing_performance == requested_performance {
                        Ok(requested_key.clone())
                    } else if global_backend == AudioBackend::DirectSound {
                        stop_worker_cues_for_restart(
                            &requested_key,
                            &workers,
                            &mut cue_owners,
                            &mut cue_generations,
                            &position_tx,
                        );
                        restart_managed_worker_with_performance(
                            &mut workers,
                            &requested_key,
                            &registry_endpoints,
                            requested_performance,
                            &mut pending_output_settings,
                            &endpoint_output_channel_settings_by_device,
                            job_supervisor.clone(),
                        )
                        .map(|_| requested_key.clone())
                    } else {
                        let in_place = workers
                            .get_mut(&requested_key)
                            .ok_or_else(|| anyhow!("Existing endpoint worker disappeared"))
                            .and_then(|worker| {
                                reconfigure_managed_worker(
                                    worker,
                                    requested_performance,
                                    Duration::from_secs(8),
                                )
                            });
                        if in_place.is_ok() {
                            Ok(requested_key.clone())
                        } else {
                            warn!(
                                "In-process performance reconfiguration failed for {} — '{}'; restarting its worker process: {:#}",
                                global_backend.short_label(),
                                device_name,
                                in_place.as_ref().unwrap_err()
                            );
                            stop_worker_cues_for_restart(
                                &requested_key,
                                &workers,
                                &mut cue_owners,
                                &mut cue_generations,
                                &position_tx,
                            );
                            restart_managed_worker_with_performance(
                                &mut workers,
                                &requested_key,
                                &registry_endpoints,
                                requested_performance,
                                &mut pending_output_settings,
                                &endpoint_output_channel_settings_by_device,
                                job_supervisor.clone(),
                            )
                            .map(|_| requested_key.clone())
                        }
                    }
                } else {
                    let config = WorkerStartupConfig {
                        protocol_version: WORKER_PROTOCOL_VERSION,
                        backend: global_backend,
                        selected_device_id: Some(endpoint_id.clone()),
                        selected_device: Some(device_name.clone()),
                        performance: requested_performance,
                        include_full_device_metadata: true,
                        registry_endpoints: Some(registry_endpoints.clone()),
                    };
                    spawn_cue_endpoint_worker(config, job_supervisor.clone()).and_then(
                        |(spawned, requested_profile)| {
                            if !worker_identity_matches_request(
                                &spawned.metadata,
                                global_backend,
                                &endpoint_id,
                                &device_name,
                            ) {
                                let actual_backend = spawned.metadata.backend;
                                let actual_name = spawned.metadata.current_device.clone();
                                let actual_id = spawned.metadata.current_device_id.clone();
                                drop(spawned);
                                return Err(anyhow!(
                                    "Requested {} endpoint '{}' [{}], but the worker resolved {} '{}' [{}]. Refusing to switch the global output to a different endpoint.",
                                    global_backend.short_label(),
                                    device_name,
                                    endpoint_id,
                                    actual_backend.short_label(),
                                    actual_name,
                                    actual_id
                                ));
                            }
                            let worker = ManagedAudioWorker::from_spawned(
                                spawned,
                                Some(&endpoint_id),
                                requested_profile,
                            );
                            let actual_key =
                                endpoint_worker_key(worker.backend, &worker.endpoint_id);
                            workers.insert(actual_key.clone(), worker);
                            if let Some(worker) = workers.get(&actual_key) {
                                apply_pending_output_settings(
                                    worker,
                                    &mut pending_output_settings,
                                );
                                apply_saved_endpoint_output_settings(
                                    worker,
                                    &endpoint_output_channel_settings_by_device,
                                );
                            }
                            Ok(actual_key)
                        },
                    )
                };
                match result {
                    Ok(key) => {
                        global_key = key.clone();
                        recovery_profiles.remove(&key);
                        recovery_retry_after.remove(&key);
                        recovery_attempts.remove(&key);
                        send_recovered_cue_states(
                            &key,
                            &mut recovery_cues,
                            &cue_generations,
                            &position_tx,
                        );
                        if let Some(worker) = workers.get(&key) {
                            global_device_name = worker.device_name.clone();
                            global_endpoint_id = worker.endpoint_id.clone();
                            if recovery_notification_needed {
                                let _ = event_tx.send(AudioWorkerManagerEvent::EndpointRecovered {
                                    metadata: worker.metadata.clone(),
                                    is_global: true,
                                });
                            }
                            info!(
                                "Global output now uses worker {} ({} channels @ {} Hz)",
                                key, worker.output_channels, worker.sample_rate
                            );
                            let _ = reply.send(Ok((worker.output_channels, worker.sample_rate)));
                        } else {
                            let _ = reply.send(Err(format!(
                                "Audio worker for endpoint '{}' was not registered",
                                device_name
                            )));
                        }
                    }
                    Err(error) => {
                        let message = format!("{error:#}");
                        if requested_key == global_key && !workers.contains_key(&requested_key) {
                            recovery_profiles
                                .entry(requested_key.clone())
                                .or_insert_with(|| EndpointRecoveryProfile {
                                    backend: global_backend,
                                    endpoint_id: endpoint_id.clone(),
                                    device_name: device_name.clone(),
                                    performance: requested_performance,
                                });
                        }
                        if recovery_profiles.contains_key(&requested_key)
                            && !recovering_endpoints.contains(&requested_key)
                            && !workers.contains_key(&requested_key)
                        {
                            let (attempt, retry) = schedule_manual_recovery_retry(
                                &requested_key,
                                &mut recovery_attempts,
                                &mut recovery_retry_after,
                            );
                            let (retry_scheduled, recovery_message) = match retry {
                                RecoveryRetryPlan::Scheduled {
                                    next_attempt,
                                    delay,
                                } => (
                                    true,
                                    format!(
                                        "{message}. Recovery check {next_attempt}/{} is scheduled in {}s after manual attempt {attempt}/{}; a Worker will only be created if the endpoint is available.",
                                        MAX_AUTOMATIC_RECOVERY_ATTEMPTS,
                                        delay.as_secs(),
                                        MAX_AUTOMATIC_RECOVERY_ATTEMPTS
                                    ),
                                ),
                                RecoveryRetryPlan::Exhausted { attempts } => (
                                    false,
                                    format!(
                                        "{message}. Automatic recovery stopped after {attempts} attempts; press GO or select the output again to retry manually."
                                    ),
                                ),
                            };
                            let _ =
                                event_tx.send(AudioWorkerManagerEvent::EndpointRecoveryFailed {
                                    backend: global_backend,
                                    endpoint_id: endpoint_id.clone(),
                                    device_name: device_name.clone(),
                                    is_global: true,
                                    error: recovery_message,
                                    retry_scheduled,
                                });
                        }
                        let _ = reply.send(Err(message));
                    }
                }
            }
            Ok(AudioCommand::ReconfigureOutput {
                performance: requested,
                reply,
            }) => {
                let requested = AudioPerformanceSettings::for_backend(global_backend, requested);
                let current_worker_state = workers.get(&global_key).map(|worker| {
                    (
                        worker.backend,
                        worker.performance_settings,
                        worker.output_channels,
                        worker.sample_rate,
                    )
                });
                let result: std::result::Result<(usize, u32), String> = match current_worker_state {
                    Some((_, current, channels, sample_rate)) if current == requested => {
                        Ok((channels, sample_rate))
                    }
                    Some((AudioBackend::DirectSound, _, _, _)) => {
                        stop_worker_cues_for_restart(
                            &global_key,
                            &workers,
                            &mut cue_owners,
                            &mut cue_generations,
                            &position_tx,
                        );
                        restart_managed_worker_with_performance(
                            &mut workers,
                            &global_key,
                            &registry_endpoints,
                            requested,
                            &mut pending_output_settings,
                            &endpoint_output_channel_settings_by_device,
                            job_supervisor.clone(),
                        )
                        .map_err(|error| format!("{error:#}"))
                    }
                    Some((_, _, _, _)) => {
                        let in_place = workers
                            .get_mut(&global_key)
                            .ok_or_else(|| anyhow!("Global audio worker is unavailable"))
                            .and_then(|worker| {
                                reconfigure_managed_worker(
                                    worker,
                                    requested,
                                    Duration::from_secs(8),
                                )
                            });
                        match in_place {
                            Ok(result) => Ok(result),
                            Err(reconfigure_error) => {
                                warn!(
                                    "In-process global output reconfiguration failed; restarting worker process: {:#}",
                                    reconfigure_error
                                );
                                stop_worker_cues_for_restart(
                                    &global_key,
                                    &workers,
                                    &mut cue_owners,
                                    &mut cue_generations,
                                    &position_tx,
                                );
                                restart_managed_worker_with_performance(
                                    &mut workers,
                                    &global_key,
                                    &registry_endpoints,
                                    requested,
                                    &mut pending_output_settings,
                                    &endpoint_output_channel_settings_by_device,
                                    job_supervisor.clone(),
                                )
                                .map_err(|restart_error| {
                                    format!(
                                        "In-process reconfiguration failed: {reconfigure_error:#}; worker restart failed: {restart_error:#}"
                                    )
                                })
                            }
                        }
                    }
                    None if recovering_endpoints.contains(&global_key) => Err(
                        "Global audio endpoint is still being recovered; wait and try again."
                            .to_string(),
                    ),
                    None => {
                        info!(
                            "Global output worker is missing; attempting manual recovery for {} — '{}' [{}]",
                            global_backend.short_label(),
                            global_device_name,
                            global_endpoint_id
                        );
                        recovery_profiles
                            .entry(global_key.clone())
                            .or_insert_with(|| EndpointRecoveryProfile {
                                backend: global_backend,
                                endpoint_id: global_endpoint_id.clone(),
                                device_name: global_device_name.clone(),
                                performance: requested,
                            });
                        let endpoint_available = registry_endpoints.iter().any(|endpoint| {
                            endpoint.backend == global_backend
                                && endpoint.id == global_endpoint_id
                                && endpoint.availability.is_available()
                        });
                        if !endpoint_available {
                            Err(format!(
                                "{} — '{}' is not currently available",
                                global_backend.short_label(),
                                global_device_name
                            ))
                        } else {
                            match spawn_managed_endpoint_worker(
                                global_backend,
                                &global_endpoint_id,
                                &global_device_name,
                                &registry_endpoints,
                                requested,
                                job_supervisor.clone(),
                            ) {
                                Ok(worker) => {
                                    let metadata = worker.metadata.clone();
                                    let recovered_device_name = worker.device_name.clone();
                                    let recovered_endpoint_id = worker.endpoint_id.clone();
                                    let channels = worker.output_channels;
                                    let sample_rate = worker.sample_rate;
                                    workers.insert(global_key.clone(), worker);
                                    if let Some(worker) = workers.get(&global_key) {
                                        apply_pending_output_settings(
                                            worker,
                                            &mut pending_output_settings,
                                        );
                                        apply_saved_endpoint_output_settings(
                                            worker,
                                            &endpoint_output_channel_settings_by_device,
                                        );
                                    }
                                    recovery_profiles.remove(&global_key);
                                    recovery_retry_after.remove(&global_key);
                                    recovery_attempts.remove(&global_key);
                                    send_recovered_cue_states(
                                        &global_key,
                                        &mut recovery_cues,
                                        &cue_generations,
                                        &position_tx,
                                    );
                                    global_device_name = recovered_device_name;
                                    global_endpoint_id = recovered_endpoint_id;
                                    let _ =
                                        event_tx.send(AudioWorkerManagerEvent::EndpointRecovered {
                                            metadata,
                                            is_global: true,
                                        });
                                    Ok((channels, sample_rate))
                                }
                                Err(error) => Err(format!("{error:#}")),
                            }
                        }
                    }
                };
                if let Err(message) = &result {
                    if !workers.contains_key(&global_key)
                        && !recovering_endpoints.contains(&global_key)
                    {
                        recovery_profiles
                            .entry(global_key.clone())
                            .or_insert_with(|| EndpointRecoveryProfile {
                                backend: global_backend,
                                endpoint_id: global_endpoint_id.clone(),
                                device_name: global_device_name.clone(),
                                performance: requested,
                            });
                        let (attempt, retry) = schedule_manual_recovery_retry(
                            &global_key,
                            &mut recovery_attempts,
                            &mut recovery_retry_after,
                        );
                        let (retry_scheduled, recovery_message) = match retry {
                            RecoveryRetryPlan::Scheduled {
                                next_attempt,
                                delay,
                            } => (
                                true,
                                format!(
                                    "{message}. Recovery check {next_attempt}/{} is scheduled in {}s after manual attempt {attempt}/{}; a Worker will only be created if the endpoint is available.",
                                    MAX_AUTOMATIC_RECOVERY_ATTEMPTS,
                                    delay.as_secs(),
                                    MAX_AUTOMATIC_RECOVERY_ATTEMPTS
                                ),
                            ),
                            RecoveryRetryPlan::Exhausted { attempts } => (
                                false,
                                format!(
                                    "{message}. Automatic recovery stopped after {attempts} attempts; reconnect the device or press GO to retry manually."
                                ),
                            ),
                        };
                        let _ = event_tx.send(AudioWorkerManagerEvent::EndpointRecoveryFailed {
                            backend: global_backend,
                            endpoint_id: global_endpoint_id.clone(),
                            device_name: global_device_name.clone(),
                            is_global: true,
                            error: recovery_message,
                            retry_scheduled,
                        });
                    }
                }
                let _ = reply.send(result);
            }
            Ok(AudioCommand::SetInputDevice(name)) => {
                send_to_worker(&workers, &global_key, AudioCommand::SetInputDevice(name));
            }
            Ok(AudioCommand::TestOutput {
                device_name,
                logical_channel,
                channel,
            }) => {
                send_to_worker(
                    &workers,
                    &global_key,
                    AudioCommand::TestOutput {
                        device_name,
                        logical_channel,
                        channel,
                    },
                );
            }
            Ok(AudioCommand::SetRouting { id, routes }) => {
                let key = cue_owners
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| global_key.clone());
                send_to_worker(&workers, &key, AudioCommand::SetRouting { id, routes });
            }
            Ok(AudioCommand::SetOutputGains { id, gains_db }) => {
                let key = cue_owners
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| global_key.clone());
                send_to_worker(
                    &workers,
                    &key,
                    AudioCommand::SetOutputGains { id, gains_db },
                );
            }
            Ok(AudioCommand::SetOutputChannelSettings {
                device_name,
                settings,
            }) => {
                if let Some(worker) = workers.values().find(|worker| {
                    worker.backend == global_backend && worker.device_name == device_name
                }) {
                    let _ = worker
                        .command_tx
                        .send(AudioCommand::SetOutputChannelSettings {
                            device_name,
                            settings,
                        });
                } else {
                    pending_output_settings.insert(
                        endpoint_preferences_key(global_backend, &device_name),
                        settings,
                    );
                }
            }
            Ok(AudioCommand::UpdateDeviceRegistry { endpoints }) => {
                let previous_endpoints = registry_endpoints.clone();
                let disconnected: Vec<AudioEndpoint> = previous_endpoints
                    .iter()
                    .filter(|previous| previous.availability.is_available())
                    .filter_map(|previous| {
                        endpoints
                            .iter()
                            .find(|current| current.id == previous.id)
                            .filter(|current| !current.availability.is_available())
                            .map(|_| previous.clone())
                    })
                    .collect();
                let reconnected: Vec<AudioEndpoint> = previous_endpoints
                    .iter()
                    .filter(|previous| !previous.availability.is_available())
                    .filter_map(|previous| {
                        endpoints
                            .iter()
                            .find(|current| current.id == previous.id)
                            .filter(|current| current.availability.is_available())
                            .cloned()
                    })
                    .collect();

                registry_endpoints = endpoints;

                for endpoint in disconnected {
                    let key = endpoint_worker_key(endpoint.backend, &endpoint.id);
                    let Some(mut worker) = workers.remove(&key) else {
                        continue;
                    };
                    let is_global = key == global_key;
                    let profile = EndpointRecoveryProfile {
                        backend: worker.backend,
                        endpoint_id: worker.endpoint_id.clone(),
                        device_name: worker.device_name.clone(),
                        performance: worker.requested_performance_settings,
                    };
                    let affected: Vec<(Uuid, u64)> = cue_owners
                        .iter()
                        .filter(|(_, owner)| owner.as_str() == key.as_str())
                        .filter_map(|(cue_id, _)| {
                            cue_generations
                                .get(cue_id)
                                .copied()
                                .map(|generation| (*cue_id, generation))
                        })
                        .collect();

                    for (cue_id, generation) in &affected {
                        cue_owners.remove(cue_id);
                        cue_generations.remove(cue_id);
                        let _ = position_tx.try_send(PositionUpdate {
                            id: *cue_id,
                            generation: *generation,
                            position_secs: 0.0,
                            state: CueState::Error(format!(
                                "{} — '{}' disconnected. Playback stopped; reconnect the device and press GO to resume.",
                                endpoint.backend.short_label(),
                                endpoint.device_name
                            )),
                        });
                    }
                    if !affected.is_empty() {
                        recovery_cues.insert(key.clone(), affected.clone());
                    }
                    recovery_profiles.insert(key.clone(), profile.clone());
                    recovery_retry_after.remove(&key);
                    recovery_attempts.insert(key.clone(), 0);
                    let first_attempt = schedule_next_recovery_attempt(
                        &key,
                        &recovery_attempts,
                        &mut recovery_retry_after,
                    );
                    if let RecoveryRetryPlan::Scheduled {
                        next_attempt,
                        delay,
                    } = first_attempt
                    {
                        info!(
                            "Device-loss recovery cycle started: endpoint={}, backend={}, device='{}'; attempt {}/{} scheduled in {}s. Availability will be checked before any Worker is created.",
                            key,
                            profile.backend.short_label(),
                            profile.device_name,
                            next_attempt,
                            MAX_AUTOMATIC_RECOVERY_ATTEMPTS,
                            delay.as_secs()
                        );
                    }
                    let affected_cue_ids = affected
                        .iter()
                        .take(32)
                        .map(|(cue_id, _)| cue_id.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    let omitted_cue_ids = affected.len().saturating_sub(32);
                    info!(
                        "Isolated lost endpoint worker: endpoint={}, backend={}, device='{}', affected_cues={}, affected_cue_ids=[{}], omitted_cue_ids={}, global={}",
                        key,
                        endpoint.backend.short_label(),
                        endpoint.device_name,
                        affected.len(),
                        affected_cue_ids,
                        omitted_cue_ids,
                        is_global
                    );
                    let worker_health = worker.health.clone();
                    if let Err(error) = worker._process.request_force_termination(&worker_health) {
                        warn!(
                            "Could not immediately terminate lost endpoint worker {}: {:#}",
                            key, error
                        );
                    }
                    let cleanup_name =
                        format!("tasq-audio-lost-cleanup-{}", endpoint.backend.short_label());
                    if let Err(error) = thread::Builder::new()
                        .name(cleanup_name)
                        .spawn(move || drop(worker))
                    {
                        warn!(
                            "Could not start background cleanup for lost endpoint worker {}: {}; worker cleanup will be handled by process teardown",
                            key, error
                        );
                    }
                    let _ = event_tx.send(AudioWorkerManagerEvent::EndpointLost {
                        backend: endpoint.backend,
                        endpoint_id: endpoint.id,
                        device_name: endpoint.device_name,
                        is_global,
                        affected_cues: affected.len(),
                    });
                }

                for endpoint in reconnected {
                    let key = endpoint_worker_key(endpoint.backend, &endpoint.id);
                    let Some(profile) = recovery_profiles.get_mut(&key) else {
                        continue;
                    };
                    profile.device_name = endpoint.device_name.clone();
                    if workers.contains_key(&key) || recovering_endpoints.contains(&key) {
                        continue;
                    }

                    match schedule_recovery_after_reconnect(
                        &key,
                        &mut recovery_attempts,
                        &mut recovery_retry_after,
                    ) {
                        RecoveryRetryPlan::Scheduled {
                            next_attempt,
                            delay,
                        } => info!(
                            "Previously managed endpoint reconnected; starting a fresh recovery cycle and scheduling attempt {}/{} in {}s: {} — '{}' [{}]",
                            next_attempt,
                            MAX_AUTOMATIC_RECOVERY_ATTEMPTS,
                            delay.as_secs(),
                            endpoint.backend.short_label(),
                            endpoint.device_name,
                            endpoint.id
                        ),
                        RecoveryRetryPlan::Exhausted { attempts } => error!(
                            "Could not schedule recovery after reconnect for {} — '{}': attempt budget exhausted ({}/{})",
                            endpoint.backend.short_label(),
                            endpoint.device_name,
                            attempts,
                            MAX_AUTOMATIC_RECOVERY_ATTEMPTS
                        ),
                    }
                }
            }
            Ok(AudioCommand::SetEndpointPreferences {
                performance_settings_by_device,
                output_channel_settings_by_device,
            }) => {
                endpoint_performance_settings_by_device =
                    normalize_endpoint_performance_profiles(performance_settings_by_device);
                endpoint_output_channel_settings_by_device = output_channel_settings_by_device;

                for worker in workers.values() {
                    apply_saved_endpoint_output_settings(
                        worker,
                        &endpoint_output_channel_settings_by_device,
                    );
                }

                info!(
                    "Updated endpoint preference profiles: {} performance profile(s), {} channel calibration profile(s)",
                    endpoint_performance_settings_by_device.len(),
                    endpoint_output_channel_settings_by_device.len()
                );
            }
            Ok(AudioCommand::Shutdown) => break,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }

    info!(
        "Audio worker manager shutdown started: {} endpoint worker(s)",
        workers.len()
    );
    for (key, worker) in &workers {
        info!(
            "Requesting shutdown for endpoint worker: endpoint={}, backend={}, device='{}', pid={}",
            key,
            worker.backend.short_label(),
            worker.device_name,
            worker.pid
        );
        match worker.command_tx.try_send(AudioCommand::Shutdown) {
            Ok(()) => {}
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                warn!(
                    "Shutdown command queue is full for endpoint worker {}; process will be force-terminated if it does not exit",
                    key
                );
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                debug!(
                    "Endpoint worker {} command channel was already disconnected during shutdown",
                    key
                );
            }
        }
    }
    // AudioWorkerProcess::drop uses bounded polling and never calls blocking
    // Child::wait after force termination. The GUI-side Job Object is the final
    // safety net if a native driver prevents a worker process from exiting.
    info!("Releasing endpoint worker processes");
    drop(workers);
    // Also terminate any child that may still be initializing inside a detached
    // recovery thread. The thread observes shutdown and drops its eventual result.
    job_supervisor.terminate_all();
    info!("Audio worker manager stopped cleanly");
}

fn apply_pending_output_settings(
    worker: &ManagedAudioWorker,
    pending: &mut HashMap<String, Vec<OutputChannelSetting>>,
) {
    let settings_key = format!("{}::{}", worker.backend.label(), worker.device_name);
    if let Some(settings) = pending.remove(&settings_key) {
        let _ = worker
            .command_tx
            .send(AudioCommand::SetOutputChannelSettings {
                device_name: worker.device_name.clone(),
                settings,
            });
    }
}

fn send_to_worker(workers: &HashMap<String, ManagedAudioWorker>, key: &str, command: AudioCommand) {
    if let Some(worker) = workers.get(key) {
        if let Err(error) = worker.command_tx.send(command) {
            warn!(
                "Could not send command to endpoint worker {} ({} — {}): {}",
                key,
                worker.backend.short_label(),
                worker.device_name,
                error
            );
        }
    } else {
        warn!(
            "Audio endpoint worker '{}' is not running; command was ignored",
            key
        );
    }
}

fn drain_worker_output_meters(
    workers: &mut HashMap<String, ManagedAudioWorker>,
    event_tx: &Sender<AudioWorkerManagerEvent>,
    next_event_at: &mut Instant,
) {
    let now = Instant::now();
    for worker in workers.values_mut() {
        while let Ok(peak) = worker.output_meter_rx.try_recv() {
            worker.latest_output_peak = if peak.is_finite() { peak.max(0.0) } else { 0.0 };
            worker.last_output_meter_at = now;
        }
        // A disconnected or recovering endpoint must not leave a stale level
        // frozen on the GUI meter.
        if now.saturating_duration_since(worker.last_output_meter_at) > Duration::from_millis(350) {
            worker.latest_output_peak = 0.0;
        }
    }

    if now >= *next_event_at {
        let peak = workers
            .values()
            .map(|worker| worker.latest_output_peak)
            .fold(0.0_f32, f32::max);
        let _ = event_tx.send(AudioWorkerManagerEvent::OutputMeter { peak });
        *next_event_at = now + Duration::from_millis(100);
    }
}

fn position_update_matches_owner(
    update: &PositionUpdate,
    worker_key: &str,
    owner_key: Option<&String>,
    owner_generation: Option<&u64>,
) -> bool {
    owner_key.is_some_and(|owner| owner.as_str() == worker_key)
        && owner_generation.is_some_and(|generation| *generation == update.generation)
}

fn drain_worker_positions(
    workers: &HashMap<String, ManagedAudioWorker>,
    cue_owners: &mut HashMap<Uuid, String>,
    cue_generations: &mut HashMap<Uuid, u64>,
    position_tx: &Sender<PositionUpdate>,
) {
    let mut completed = Vec::new();
    for (key, worker) in workers {
        while let Ok(update) = worker.position_rx.try_recv() {
            // A Cue may be restarted on the same endpoint before the previous
            // generation's final Stopped event reaches this manager. Ignore
            // stale events for both UI delivery and ownership cleanup.
            if !position_update_matches_owner(
                &update,
                key,
                cue_owners.get(&update.id),
                cue_generations.get(&update.id),
            ) {
                continue;
            }
            if matches!(&update.state, CueState::Stopped | CueState::Error(_)) {
                completed.push((update.id, key.clone(), update.generation));
            }
            let _ = position_tx.try_send(update);
        }
    }
    for (cue_id, key, generation) in completed {
        if cue_owners.get(&cue_id) == Some(&key)
            && cue_generations.get(&cue_id) == Some(&generation)
        {
            cue_owners.remove(&cue_id);
            cue_generations.remove(&cue_id);
        }
    }
}

fn write_json_line(writer: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    writer.write_all(b"\n")?;
    writer.flush()
}

pub(crate) fn run_worker_child_stdio() -> Result<()> {
    // Worker stdout is the IPC channel. All diagnostics are deliberately sent
    // to stderr and relayed into the parent process log.
    let _ = tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter("TaSQ=debug,TheatreShowCue=debug,warn")
        .try_init();

    let stdin = io::stdin();
    let mut initial_reader = BufReader::new(stdin.lock());
    let mut initial_line = String::new();
    if initial_reader.read_line(&mut initial_line)? == 0 {
        anyhow::bail!("Audio worker did not receive its initialization message");
    }
    let initial_request: WorkerRequest = serde_json::from_str(&initial_line)
        .context("Could not decode audio worker initialization message")?;
    drop(initial_reader);

    let config = match initial_request {
        WorkerRequest::Initialize(config) => config,
        other => anyhow::bail!(
            "Audio worker expected Initialize as its first message, received {:?}",
            other
        ),
    };
    if config.protocol_version != WORKER_PROTOCOL_VERSION {
        anyhow::bail!(
            "Audio worker protocol mismatch: expected {}, received {}",
            WORKER_PROTOCOL_VERSION,
            config.protocol_version
        );
    }

    let registry = match config.registry_endpoints {
        Some(endpoints) => {
            if !config.include_full_device_metadata {
                info!(
                    "GO_DEVICE_REGISTRY: using cached snapshot with {} endpoint(s); device enumeration skipped",
                    endpoints.len()
                );
            }
            crate::audio::device_registry::DeviceRegistry::from_endpoints(endpoints)
        }
        None if !config.include_full_device_metadata => {
            anyhow::bail!(
                "Per-Cue audio Worker requires a cached device-registry snapshot; device enumeration is disabled on the GO path"
            );
        }
        None => crate::audio::device_registry::DeviceRegistry::from_scan(
            AudioEngine::scan_output_endpoints(),
        ),
    };

    let mut engine =
        match AudioEngine::new_in_process_with_backend_and_endpoint_using_registry_and_performance(
            config.backend,
            config.selected_device_id.as_deref(),
            config.selected_device.as_deref(),
            registry,
            config.performance,
            config.include_full_device_metadata,
        ) {
            Ok(engine) => engine,
            Err(error) => {
                let mut writer = BufWriter::new(io::stdout().lock());
                write_json_line(
                    &mut writer,
                    &WorkerEvent::Ready {
                        protocol_version: WORKER_PROTOCOL_VERSION,
                        result: Err(format!("{error:#}")),
                    },
                )?;
                return Ok(());
            }
        };

    let metadata = engine.worker_metadata();
    let stdout = io::stdout();
    let mut writer = BufWriter::new(stdout.lock());
    write_json_line(
        &mut writer,
        &WorkerEvent::Ready {
            protocol_version: WORKER_PROTOCOL_VERSION,
            result: Ok(metadata),
        },
    )?;

    let (request_tx, request_rx) = unbounded::<std::result::Result<WorkerRequest, String>>();
    thread::Builder::new()
        .name("tasq-audio-worker-stdin".to_string())
        .spawn(move || {
            let stdin = io::stdin();
            let reader = BufReader::new(stdin.lock());
            for line in reader.lines() {
                match line {
                    Ok(line) if line.trim().is_empty() => {}
                    Ok(line) => {
                        let parsed = serde_json::from_str::<WorkerRequest>(&line)
                            .map_err(|error| format!("Invalid worker request: {error}"));
                        if request_tx.send(parsed).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = request_tx.send(Err(format!("Worker stdin read error: {error}")));
                        break;
                    }
                }
            }
        })
        .context("Could not start audio worker stdin reader")?;

    // This heartbeat measures responsiveness of the worker control loop; it
    // does not claim that the native device is currently producing audio.
    let mut heartbeat_sequence = 0_u64;
    let mut next_heartbeat = Instant::now() + WORKER_HEARTBEAT_INTERVAL;
    let mut next_output_meter = Instant::now() + Duration::from_millis(100);
    loop {
        match request_rx.recv_timeout(Duration::from_millis(5)) {
            Ok(Ok(WorkerRequest::Command {
                protocol_version,
                request_id,
                command,
            })) => {
                if protocol_version != WORKER_PROTOCOL_VERSION {
                    write_json_line(
                        &mut writer,
                        &WorkerEvent::ProtocolError {
                            message: format!(
                                "Protocol mismatch on command: parent={}, child={}",
                                protocol_version, WORKER_PROTOCOL_VERSION
                            ),
                        },
                    )?;
                } else if handle_worker_command(command, request_id, &mut engine, &mut writer)? {
                    break;
                }
            }
            Ok(Ok(WorkerRequest::Initialize(_))) => {
                write_json_line(
                    &mut writer,
                    &WorkerEvent::ProtocolError {
                        message: "Initialize may only be sent once".to_string(),
                    },
                )?;
            }
            Ok(Err(message)) => {
                write_json_line(&mut writer, &WorkerEvent::ProtocolError { message })?;
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        for update in engine.drain_position_updates() {
            write_json_line(&mut writer, &WorkerEvent::Position(update))?;
        }

        if engine.audio_thread_finished() {
            let message = "Audio render thread exited unexpectedly after a native output-stream failure or panic".to_string();
            write_json_line(
                &mut writer,
                &WorkerEvent::ProtocolError {
                    message: message.clone(),
                },
            )?;
            // Do not send WorkerEvent::Stopped here: the parent supervisor must
            // classify this as an unhealthy Worker and run endpoint recovery.
            engine.shutdown_and_wait();
            return Err(anyhow!(message));
        }

        let now = Instant::now();
        if now >= next_output_meter {
            write_json_line(
                &mut writer,
                &WorkerEvent::OutputMeter {
                    peak: engine.take_output_peak(),
                },
            )?;
            next_output_meter = now + Duration::from_millis(100);
        }

        if now >= next_heartbeat {
            heartbeat_sequence = heartbeat_sequence.wrapping_add(1);
            write_json_line(
                &mut writer,
                &WorkerEvent::Heartbeat {
                    sequence: heartbeat_sequence,
                },
            )?;
            next_heartbeat = now + WORKER_HEARTBEAT_INTERVAL;
        }
    }

    // Do not acknowledge shutdown until the native stream thread has exited.
    // The parent supervises the process and can terminate it if a driver hangs.
    engine.shutdown_and_wait();
    write_json_line(&mut writer, &WorkerEvent::Stopped)?;
    Ok(())
}

fn handle_worker_command(
    command: WorkerCommand,
    request_id: u64,
    engine: &mut AudioEngine,
    writer: &mut impl Write,
) -> Result<bool> {
    let response = match command {
        WorkerCommand::Play {
            id,
            generation,
            file,
            output_device,
            start_secs,
            end_secs,
            duration_secs,
            volume,
            volume_automation_enabled,
            volume_automation,
            time_pitch,
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
            engine.send(AudioCommand::Play {
                id,
                generation,
                file,
                output_device,
                output_backend: None,
                output_device_id: None,
                start_secs,
                end_secs,
                duration_secs,
                volume,
                volume_automation_enabled,
                volume_automation,
                time_pitch,
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
            });
            None
        }
        WorkerCommand::PlayBatch {
            transaction_id,
            cues,
        } => {
            engine.send(AudioCommand::PlayBatch {
                transaction_id,
                cues,
            });
            None
        }
        WorkerCommand::Stop(id) => {
            engine.send(AudioCommand::Stop(id));
            None
        }
        WorkerCommand::StopTimed {
            id,
            request_id,
            requested_at_unix_micros,
        } => {
            engine.send(AudioCommand::StopTimed {
                id,
                request_id,
                requested_at_unix_micros,
            });
            None
        }
        WorkerCommand::StopAll => {
            engine.send(AudioCommand::StopAll);
            None
        }
        WorkerCommand::StopAllTimed {
            request_id,
            requested_at_unix_micros,
        } => {
            engine.send(AudioCommand::StopAllTimed {
                request_id,
                requested_at_unix_micros,
            });
            None
        }
        WorkerCommand::Pause(id) => {
            engine.send(AudioCommand::Pause(id));
            None
        }
        WorkerCommand::Seek { id, position_secs } => {
            engine.send(AudioCommand::Seek { id, position_secs });
            None
        }
        WorkerCommand::PauseAll => {
            engine.send(AudioCommand::PauseAll);
            None
        }
        WorkerCommand::Resume(id) => {
            engine.send(AudioCommand::Resume(id));
            None
        }
        WorkerCommand::ResumeAll => {
            engine.send(AudioCommand::ResumeAll);
            None
        }
        WorkerCommand::SetVolume(id, volume) => {
            engine.send(AudioCommand::SetVolume(id, volume));
            None
        }
        WorkerCommand::SetVolumeAutomation {
            id,
            enabled,
            points,
        } => {
            engine.send(AudioCommand::SetVolumeAutomation {
                id,
                enabled,
                points,
            });
            None
        }
        WorkerCommand::SetTimePitch { id, settings } => {
            engine.send(AudioCommand::SetTimePitch { id, settings });
            None
        }
        WorkerCommand::Fade {
            id,
            target_volume,
            duration_secs,
            stop_on_finish,
            curve,
            custom_fade_points,
        } => {
            engine.send(AudioCommand::Fade {
                id,
                target_volume,
                duration_secs,
                stop_on_finish,
                curve,
                custom_fade_points,
            });
            None
        }
        WorkerCommand::SetOutputDevice {
            device_name,
            performance,
        } => {
            let endpoint_id = engine
                .registered_endpoint_id(engine.backend, &device_name)
                .unwrap_or_else(|| {
                    format!("{}:name:{}", engine.backend.short_label(), device_name)
                });
            let result = engine.set_output_endpoint(&endpoint_id, &device_name, performance);
            Some(
                result
                    .map(|_| WorkerOutputState {
                        channels: engine.output_channels,
                        sample_rate: engine.current_sample_rate,
                    })
                    .map_err(|error| format!("{error:#}")),
            )
        }
        WorkerCommand::ReconfigureOutput { performance } => {
            let result = engine.reconfigure_output(performance);
            Some(
                result
                    .map(|_| WorkerOutputState {
                        channels: engine.output_channels,
                        sample_rate: engine.current_sample_rate,
                    })
                    .map_err(|error| format!("{error:#}")),
            )
        }
        WorkerCommand::SetInputDevice(name) => {
            engine.set_input_device(&name);
            None
        }
        WorkerCommand::TestOutput {
            device_name,
            logical_channel,
            channel,
        } => {
            engine.send(AudioCommand::TestOutput {
                device_name,
                logical_channel,
                channel,
            });
            None
        }
        WorkerCommand::SetRouting { id, routes } => {
            engine.send(AudioCommand::SetRouting { id, routes });
            None
        }
        WorkerCommand::SetOutputGains { id, gains_db } => {
            engine.send(AudioCommand::SetOutputGains { id, gains_db });
            None
        }
        WorkerCommand::SetOutputChannelSettings {
            device_name,
            settings,
        } => {
            engine.set_output_channel_settings_for_device(&device_name, settings);
            None
        }
        WorkerCommand::Shutdown => {
            return Ok(true);
        }
    };

    if let Some(result) = response {
        if request_id != 0 {
            write_json_line(writer, &WorkerEvent::Response { request_id, result })?;
        }
    }
    Ok(false)
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    use crate::cue::CueState;

    #[test]
    fn worker_startup_config_round_trips_endpoint_snapshot() {
        let request = WorkerRequest::Initialize(WorkerStartupConfig {
            protocol_version: WORKER_PROTOCOL_VERSION,
            backend: AudioBackend::Wasapi,
            selected_device_id: Some("wasapi:creative-speakers".to_string()),
            selected_device: Some("Creative Speakers".to_string()),
            performance: AudioPerformanceSettings::default(),
            include_full_device_metadata: true,
            registry_endpoints: Some(vec![AudioEndpoint {
                id: "wasapi:creative-speakers".to_string(),
                backend: AudioBackend::Wasapi,
                device_name: "Creative Speakers".to_string(),
                channels: 2,
                channel_layouts: vec![2],
                availability: crate::audio::device_registry::DeviceAvailability::Available,
            }]),
        });
        let encoded = serde_json::to_string(&request).expect("startup request serializes");
        let decoded: WorkerRequest =
            serde_json::from_str(&encoded).expect("startup request deserializes");
        match decoded {
            WorkerRequest::Initialize(config) => {
                assert_eq!(config.backend, AudioBackend::Wasapi);
                let endpoints = config.registry_endpoints.expect("endpoint snapshot");
                assert_eq!(endpoints.len(), 1);
                assert_eq!(endpoints[0].id, "wasapi:creative-speakers");
            }
            _ => panic!("expected worker initialization request"),
        }
    }

    #[test]
    fn worker_position_event_round_trips() {
        let event = WorkerEvent::Position(PositionUpdate {
            id: Uuid::new_v4(),
            generation: 11,
            position_secs: 3.25,
            state: CueState::Playing,
        });
        let encoded = serde_json::to_string(&event).expect("position event serializes");
        let decoded: WorkerEvent =
            serde_json::from_str(&encoded).expect("position event deserializes");
        assert!(matches!(
            decoded,
            WorkerEvent::Position(PositionUpdate {
                generation: 11,
                position_secs: 3.25,
                state: CueState::Playing,
                ..
            })
        ));
    }

    #[test]
    fn worker_startup_message_round_trips_with_protocol_version() {
        let request = WorkerRequest::Initialize(WorkerStartupConfig {
            protocol_version: WORKER_PROTOCOL_VERSION,
            backend: AudioBackend::Asio,
            selected_device_id: Some("asio:{TEST}".to_string()),
            selected_device: Some("Test ASIO".to_string()),
            performance: AudioPerformanceSettings {
                sample_rate: Some(48_000),
                buffer_size_frames: Some(256),
            },
            include_full_device_metadata: true,
            registry_endpoints: None,
        });
        let encoded = serde_json::to_string(&request).expect("request serializes");
        let decoded: WorkerRequest = serde_json::from_str(&encoded).expect("request deserializes");
        match decoded {
            WorkerRequest::Initialize(config) => {
                assert_eq!(config.protocol_version, WORKER_PROTOCOL_VERSION);
                assert_eq!(config.backend, AudioBackend::Asio);
                assert_eq!(config.selected_device.as_deref(), Some("Test ASIO"));
                assert_eq!(config.performance.buffer_size_frames, Some(256));
            }
            _ => panic!("expected Initialize request"),
        }
    }

    #[test]
    fn worker_play_command_round_trips_all_cue_parameters() {
        let command = WorkerCommand::Play {
            id: Uuid::new_v4(),
            generation: 7,
            file: PathBuf::from("audio/test.wav"),
            output_device: Some("TASCAM ASIO".to_string()),
            start_secs: 1.25,
            end_secs: Some(8.5),
            duration_secs: 12.0,
            volume: 0.75,
            volume_automation_enabled: true,
            volume_automation: vec![[0.0, 0.0], [2.0, 1.0]],
            time_pitch: TimePitchSettings::default(),
            fade_in_secs: 0.5,
            fade_out_secs: 1.0,
            fade_curve: FadeCurve::SCurve,
            custom_fade_points: vec![[0.0, 0.0], [1.0, 1.0]],
            channel_routes: vec![ChannelRoute::default()],
            output_gains_db: vec![0.0, -3.0],
            loop_enabled: false,
            loop_start: 0.0,
            loop_end: 0.0,
            loop_count: -1,
        };
        let encoded = serde_json::to_string(&command).expect("command serializes");
        let decoded: WorkerCommand = serde_json::from_str(&encoded).expect("command deserializes");
        assert!(matches!(decoded, WorkerCommand::Play { generation: 7, .. }));
    }

    #[test]
    fn worker_output_channel_settings_command_round_trips() {
        let command = WorkerCommand::SetOutputChannelSettings {
            device_name: "Creative Sound Blaster Omni".to_string(),
            settings: vec![
                OutputChannelSetting {
                    active: true,
                    gain_db: -3.0,
                },
                OutputChannelSetting {
                    active: false,
                    gain_db: 0.0,
                },
            ],
        };
        let encoded = serde_json::to_string(&command).expect("command serializes");
        let decoded: WorkerCommand = serde_json::from_str(&encoded).expect("command deserializes");
        match decoded {
            WorkerCommand::SetOutputChannelSettings {
                device_name,
                settings,
            } => {
                assert_eq!(device_name, "Creative Sound Blaster Omni");
                assert_eq!(settings.len(), 2);
                assert_eq!(settings[0].gain_db, -3.0);
                assert!(!settings[1].active);
            }
            _ => panic!("expected output-channel settings command"),
        }
    }

    #[test]
    fn worker_response_round_trips_output_state() {
        let event = WorkerEvent::Response {
            request_id: 42,
            result: Ok(WorkerOutputState {
                channels: 6,
                sample_rate: 48_000,
            }),
        };
        let encoded = serde_json::to_string(&event).expect("event serializes");
        let decoded: WorkerEvent = serde_json::from_str(&encoded).expect("event deserializes");
        assert!(matches!(
            decoded,
            WorkerEvent::Response {
                request_id: 42,
                result: Ok(WorkerOutputState {
                    channels: 6,
                    sample_rate: 48_000
                })
            }
        ));
    }

    #[test]
    fn worker_heartbeat_event_round_trips_sequence_number() {
        let event = WorkerEvent::Heartbeat { sequence: 17 };
        let encoded = serde_json::to_string(&event).expect("heartbeat serializes");
        let decoded: WorkerEvent = serde_json::from_str(&encoded).expect("heartbeat deserializes");

        assert!(matches!(decoded, WorkerEvent::Heartbeat { sequence: 17 }));
    }

    #[test]
    fn worker_health_detects_stale_heartbeat_and_unexpected_stream_close() {
        let health = WorkerHealth::new();
        let now = health.last_heartbeat + WORKER_HEARTBEAT_TIMEOUT;

        assert_eq!(
            worker_health_issue(&health, now),
            Some(WorkerHealthIssue::HeartbeatTimeout)
        );

        let mut closed = health;
        closed.event_stream_closed = true;
        assert_eq!(
            worker_health_issue(&closed, now),
            Some(WorkerHealthIssue::EventStreamClosed)
        );
    }

    #[test]
    fn worker_health_accepts_recent_heartbeat_and_ignores_clean_shutdown() {
        let mut health = WorkerHealth::new();
        let now = health.last_heartbeat + WORKER_HEARTBEAT_TIMEOUT;
        health.record_heartbeat(5);

        assert_eq!(health.heartbeat_sequence, 5);
        assert_eq!(worker_health_issue(&health, Instant::now()), None);

        health.event_stream_closed = true;
        health.shutdown_acknowledged = true;
        assert_eq!(worker_health_issue(&health, now), None);
    }
}

#[cfg(test)]
mod worker_manager_identity_tests {
    use super::{
        endpoint_worker_key, explicit_endpoint_matches_global, position_update_matches_owner,
        worker_identity_matches_request,
    };
    use crate::audio::engine::PositionUpdate;
    use crate::audio::worker::WorkerEngineMetadata;
    use crate::config::{AudioBackend, AudioPerformanceSettings};
    use crate::cue::CueState;
    use std::collections::HashMap;
    use uuid::Uuid;

    #[test]
    fn stale_generation_does_not_match_current_cue_owner() {
        let cue_id = Uuid::new_v4();
        let worker_key = "ASIO::endpoint-a".to_string();
        let current_generation = 12_u64;
        let stale_update = PositionUpdate {
            id: cue_id,
            generation: 11,
            position_secs: 0.0,
            state: CueState::Stopped,
        };
        let current_update = PositionUpdate {
            id: cue_id,
            generation: current_generation,
            position_secs: 0.0,
            state: CueState::Playing,
        };

        assert!(!position_update_matches_owner(
            &stale_update,
            &worker_key,
            Some(&worker_key),
            Some(&current_generation),
        ));
        assert!(position_update_matches_owner(
            &current_update,
            &worker_key,
            Some(&worker_key),
            Some(&current_generation),
        ));
    }

    #[test]
    fn worker_resolution_accepts_exact_stable_id_and_exact_name_fallback_only() {
        let mut metadata = WorkerEngineMetadata {
            backend: AudioBackend::Asio,
            output_devices: Vec::new(),
            registry_endpoints: Vec::new(),
            input_devices: Vec::new(),
            current_device: "TASCAM US-366".to_string(),
            current_device_id: "cpal:asio:tascam".to_string(),
            current_input_device: String::new(),
            output_channels: 6,
            current_sample_rate: 48_000,
            performance_settings: AudioPerformanceSettings::default(),
            supported_sample_rates: Vec::new(),
            supported_buffer_sizes: Vec::new(),
            output_device_channels: HashMap::new(),
            output_device_channel_layouts: HashMap::new(),
            output_driver_channel_info: HashMap::new(),
            detected_default_output_pairs: HashMap::new(),
        };

        assert!(worker_identity_matches_request(
            &metadata,
            AudioBackend::Asio,
            "cpal:asio:tascam",
            "TASCAM US-366",
        ));
        assert!(worker_identity_matches_request(
            &metadata,
            AudioBackend::Asio,
            "asio:name:tascam-us-366",
            "TASCAM US-366",
        ));
        assert!(!worker_identity_matches_request(
            &metadata,
            AudioBackend::Asio,
            "asio:name:another-interface",
            "Another Interface",
        ));
        metadata.backend = AudioBackend::Wasapi;
        assert!(!worker_identity_matches_request(
            &metadata,
            AudioBackend::Asio,
            "cpal:asio:tascam",
            "TASCAM US-366",
        ));
    }

    #[test]
    fn explicit_global_endpoint_reuses_global_worker_by_stable_id() {
        assert!(explicit_endpoint_matches_global(
            AudioBackend::Asio,
            Some("endpoint-a"),
            Some("TASCAM US-366"),
            AudioBackend::Asio,
            "endpoint-a",
            "TASCAM US-366",
        ));
    }

    #[test]
    fn legacy_explicit_global_endpoint_reuses_global_worker_by_exact_name() {
        assert!(explicit_endpoint_matches_global(
            AudioBackend::Asio,
            None,
            Some("TASCAM US-366"),
            AudioBackend::Asio,
            "endpoint-a",
            "TASCAM US-366",
        ));
    }

    #[test]
    fn explicit_route_with_different_stable_id_does_not_alias_global_by_name() {
        assert!(!explicit_endpoint_matches_global(
            AudioBackend::Asio,
            Some("endpoint-b"),
            Some("TASCAM US-366"),
            AudioBackend::Asio,
            "endpoint-a",
            "TASCAM US-366",
        ));
    }

    #[test]
    fn explicit_route_on_another_backend_does_not_alias_global_worker() {
        assert!(!explicit_endpoint_matches_global(
            AudioBackend::Wasapi,
            Some("endpoint-a"),
            Some("TASCAM US-366"),
            AudioBackend::Asio,
            "endpoint-a",
            "TASCAM US-366",
        ));
    }

    #[test]
    fn worker_identity_is_scoped_by_backend_and_stable_endpoint_id() {
        let asio_a = endpoint_worker_key(AudioBackend::Asio, "endpoint-a");
        let asio_b = endpoint_worker_key(AudioBackend::Asio, "endpoint-b");
        let wasapi_a = endpoint_worker_key(AudioBackend::Wasapi, "endpoint-a");

        assert_ne!(asio_a, asio_b);
        assert_ne!(asio_a, wasapi_a);
        assert_eq!(
            asio_a,
            endpoint_worker_key(AudioBackend::Asio, "endpoint-a")
        );
    }
}

#[cfg(test)]
mod endpoint_preferences_tests {
    use super::{endpoint_preferences_key, AudioBackend};

    #[test]
    fn endpoint_preference_keys_are_scoped_by_backend_and_device_name() {
        assert_eq!(
            endpoint_preferences_key(AudioBackend::Asio, "TASCAM US-366"),
            "ASIO::TASCAM US-366"
        );
        assert_eq!(
            endpoint_preferences_key(AudioBackend::Wasapi, "TASCAM US-366"),
            format!("{}::TASCAM US-366", AudioBackend::Wasapi.label())
        );
        assert_ne!(
            endpoint_preferences_key(AudioBackend::Asio, "TASCAM US-366"),
            endpoint_preferences_key(AudioBackend::Wasapi, "TASCAM US-366")
        );
    }
}

#[cfg(test)]
mod worker_shutdown_safety_tests {
    use super::{AudioWorkerJobSupervisor, WORKER_FORCE_EXIT_TIMEOUT, WORKER_SHUTDOWN_TIMEOUT};
    use std::time::Duration;

    #[test]
    fn worker_process_shutdown_waits_are_bounded() {
        assert!(WORKER_SHUTDOWN_TIMEOUT <= Duration::from_secs(3));
        assert!(WORKER_FORCE_EXIT_TIMEOUT <= Duration::from_secs(1));
    }

    #[test]
    fn empty_job_supervisor_can_be_terminated_safely() {
        AudioWorkerJobSupervisor::new().terminate_all();
    }
}

#[cfg(test)]
mod endpoint_recovery_state_tests {
    use super::send_recovered_cue_states;
    use crate::audio::engine::PositionUpdate;
    use crate::cue::CueState;
    use crossbeam_channel::bounded;
    use std::collections::HashMap;
    use uuid::Uuid;

    #[test]
    fn recovered_cues_return_to_stopped_without_auto_playback() {
        let cue_id = Uuid::new_v4();
        let generation = 27;
        let worker_key = "ASIO::test-interface".to_string();
        let (position_tx, position_rx) = bounded::<PositionUpdate>(4);
        let mut recovery_cues = HashMap::from([(worker_key.clone(), vec![(cue_id, generation)])]);
        let cue_generations = HashMap::new();

        send_recovered_cue_states(
            &worker_key,
            &mut recovery_cues,
            &cue_generations,
            &position_tx,
        );

        let update = position_rx.try_recv().expect("recovery state update");
        assert_eq!(update.id, cue_id);
        assert_eq!(update.generation, generation);
        assert_eq!(update.state, CueState::Stopped);
        assert!(recovery_cues.is_empty());
    }

    #[test]
    fn recovery_does_not_overwrite_a_newer_cue_generation() {
        let cue_id = Uuid::new_v4();
        let worker_key = "WASAPI::usb-interface".to_string();
        let (position_tx, position_rx) = bounded::<PositionUpdate>(4);
        let mut recovery_cues = HashMap::from([(worker_key.clone(), vec![(cue_id, 27)])]);
        let cue_generations = HashMap::from([(cue_id, 28)]);

        send_recovered_cue_states(
            &worker_key,
            &mut recovery_cues,
            &cue_generations,
            &position_tx,
        );

        assert!(position_rx.try_recv().is_err());
        assert!(recovery_cues.is_empty());
    }
}

#[cfg(test)]
mod recovery_retry_policy_tests {
    use super::{
        mark_recovery_attempt_started, recovery_attempt_delay, schedule_manual_recovery_retry,
        schedule_next_recovery_attempt, schedule_recovery_after_reconnect, RecoveryRetryPlan,
        MAX_AUTOMATIC_RECOVERY_ATTEMPTS,
    };
    use std::collections::HashMap;
    use std::time::Duration;

    #[test]
    fn automatic_recovery_is_limited_to_five_checks() {
        let mut attempts = HashMap::new();
        let key = "ASIO::test-interface";

        for expected in 1..=MAX_AUTOMATIC_RECOVERY_ATTEMPTS {
            assert_eq!(
                mark_recovery_attempt_started(key, &mut attempts),
                Some(expected)
            );
        }

        assert_eq!(mark_recovery_attempt_started(key, &mut attempts), None);
        assert_eq!(attempts.get(key), Some(&MAX_AUTOMATIC_RECOVERY_ATTEMPTS));
    }

    #[test]
    fn recovery_cycle_uses_one_one_two_five_ten_second_delays() {
        assert_eq!(recovery_attempt_delay(1), Some(Duration::from_secs(1)));
        assert_eq!(recovery_attempt_delay(2), Some(Duration::from_secs(1)));
        assert_eq!(recovery_attempt_delay(3), Some(Duration::from_secs(2)));
        assert_eq!(recovery_attempt_delay(4), Some(Duration::from_secs(5)));
        assert_eq!(recovery_attempt_delay(5), Some(Duration::from_secs(10)));
        assert_eq!(recovery_attempt_delay(6), None);
    }

    #[test]
    fn first_recovery_check_is_scheduled_one_second_after_cycle_starts() {
        let key = "ASIO::disconnected-interface";
        let attempts = HashMap::from([(key.to_string(), 0)]);
        let mut retry_after = HashMap::new();

        assert_eq!(
            schedule_next_recovery_attempt(key, &attempts, &mut retry_after),
            RecoveryRetryPlan::Scheduled {
                next_attempt: 1,
                delay: Duration::from_secs(1),
            }
        );
        assert!(retry_after.contains_key(key));
    }

    #[test]
    fn unavailable_device_does_not_pause_the_five_check_schedule() {
        let key = "ASIO::offline-interface";
        let mut attempts = HashMap::from([(key.to_string(), 0)]);
        let mut retry_after = HashMap::new();

        for expected_attempt in 1..=MAX_AUTOMATIC_RECOVERY_ATTEMPTS {
            let plan = schedule_next_recovery_attempt(key, &attempts, &mut retry_after);
            let delay = recovery_attempt_delay(expected_attempt).unwrap();

            assert_eq!(
                plan,
                RecoveryRetryPlan::Scheduled {
                    next_attempt: expected_attempt,
                    delay,
                }
            );
            assert!(retry_after.contains_key(key));

            assert_eq!(
                mark_recovery_attempt_started(key, &mut attempts),
                Some(expected_attempt)
            );
            retry_after.remove(key);
        }

        assert_eq!(
            schedule_next_recovery_attempt(key, &attempts, &mut retry_after),
            RecoveryRetryPlan::Exhausted { attempts: 5 }
        );
        assert!(!retry_after.contains_key(key));
    }

    #[test]
    fn fifth_failed_attempt_does_not_schedule_another_check() {
        let key = "WASAPI::test-interface";
        let attempts = HashMap::from([(key.to_string(), MAX_AUTOMATIC_RECOVERY_ATTEMPTS)]);
        let mut retry_after = HashMap::new();

        assert_eq!(
            schedule_next_recovery_attempt(key, &attempts, &mut retry_after),
            RecoveryRetryPlan::Exhausted { attempts: 5 }
        );
        assert!(!retry_after.contains_key(key));
    }

    #[test]
    fn explicit_manual_retry_starts_a_fresh_attempt_budget() {
        let key = "ASIO::manual-retry-interface";
        let mut attempts = HashMap::from([(key.to_string(), MAX_AUTOMATIC_RECOVERY_ATTEMPTS)]);
        let mut retry_after = HashMap::new();

        let (attempt, plan) = schedule_manual_recovery_retry(key, &mut attempts, &mut retry_after);

        assert_eq!(attempt, 1);
        assert_eq!(attempts.get(key), Some(&1));
        assert_eq!(
            plan,
            RecoveryRetryPlan::Scheduled {
                next_attempt: 2,
                delay: Duration::from_secs(1),
            }
        );
    }

    #[test]
    fn reconnect_after_exhaustion_starts_a_fresh_cycle() {
        let key = "ASIO::reconnected-interface";
        let mut attempts = HashMap::from([(key.to_string(), MAX_AUTOMATIC_RECOVERY_ATTEMPTS)]);
        let mut retry_after = HashMap::new();

        let plan = schedule_recovery_after_reconnect(key, &mut attempts, &mut retry_after);

        assert_eq!(attempts.get(key), Some(&0));
        assert_eq!(
            plan,
            RecoveryRetryPlan::Scheduled {
                next_attempt: 1,
                delay: Duration::from_secs(1),
            }
        );
        assert!(retry_after.contains_key(key));
    }

    #[test]
    fn reconnect_during_active_cycle_restarts_the_schedule_from_attempt_one() {
        let key = "ASIO::active-cycle-interface";
        let mut attempts = HashMap::from([(key.to_string(), 2)]);
        let mut retry_after = HashMap::from([(key.to_string(), std::time::Instant::now())]);

        let plan = schedule_recovery_after_reconnect(key, &mut attempts, &mut retry_after);

        assert_eq!(attempts.get(key), Some(&0));
        assert_eq!(
            plan,
            RecoveryRetryPlan::Scheduled {
                next_attempt: 1,
                delay: Duration::from_secs(1),
            }
        );
        assert!(retry_after.contains_key(key));
    }

    #[test]
    fn retry_schedule_covers_approximately_nineteen_seconds() {
        let total: u64 = (1..=MAX_AUTOMATIC_RECOVERY_ATTEMPTS)
            .map(|attempt| recovery_attempt_delay(attempt).unwrap().as_secs())
            .sum();

        assert_eq!(total, 19);
    }
}

#[cfg(test)]
mod play_batch_tests {
    use super::*;

    fn play_command(id: Uuid) -> AudioCommand {
        AudioCommand::Play {
            id,
            generation: 1,
            file: PathBuf::from(format!("cue-{id}.wav")),
            output_device: None,
            output_backend: None,
            output_device_id: None,
            start_secs: 0.0,
            end_secs: None,
            duration_secs: 10.0,
            volume: 1.0,
            volume_automation_enabled: false,
            volume_automation: Vec::new(),
            time_pitch: TimePitchSettings::default(),
            fade_in_secs: 0.0,
            fade_out_secs: 0.0,
            fade_curve: FadeCurve::Linear,
            custom_fade_points: Vec::new(),
            channel_routes: Vec::new(),
            output_gains_db: Vec::new(),
            loop_enabled: false,
            loop_start: 0.0,
            loop_end: 0.0,
            loop_count: 0,
        }
    }

    #[test]
    fn play_batch_ipc_payload_roundtrips() {
        let cue = audio_play_request_from_command(play_command(Uuid::new_v4()))
            .expect("Play command converts to batch payload");
        let transaction_id = Uuid::new_v4();
        let command = WorkerCommand::PlayBatch {
            transaction_id,
            cues: vec![cue],
        };

        let encoded = serde_json::to_string(&command).expect("batch serializes");
        let decoded: WorkerCommand = serde_json::from_str(&encoded).expect("batch deserializes");

        assert!(matches!(
            decoded,
            WorkerCommand::PlayBatch { transaction_id: decoded_id, cues }
                if decoded_id == transaction_id && cues.len() == 1
        ));
    }

    #[test]
    fn one_cue_launch_transaction_uses_direct_play_without_batch_preroll() {
        let transaction_id = Uuid::new_v4();
        let cue = audio_play_request_from_command(play_command(Uuid::new_v4()))
            .expect("Play converts to a batch request");

        assert!(matches!(
            worker_command_for_launch_segment(transaction_id, vec![cue]),
            WorkerCommand::Play { .. }
        ));
    }

    #[test]
    fn multiple_cues_in_one_launch_transaction_use_a_single_play_batch() {
        let transaction_id = Uuid::new_v4();
        let cues = (0..10)
            .map(|_| {
                audio_play_request_from_command(play_command(Uuid::new_v4()))
                    .expect("Play converts to a batch request")
            })
            .collect();

        assert!(matches!(
            worker_command_for_launch_segment(transaction_id, cues),
            WorkerCommand::PlayBatch { transaction_id: batch_id, cues }
                if batch_id == transaction_id && cues.len() == 10
        ));
    }

    #[test]
    fn explicit_launch_transaction_collects_all_play_commands_without_a_time_window() {
        let transaction_id = Uuid::new_v4();
        let mut batch = LaunchBatchAccumulator::default();
        assert!(batch.begin(transaction_id).is_none());

        for _ in 0..10 {
            let cue = audio_play_request_from_command(play_command(Uuid::new_v4()))
                .expect("Play converts to a batch request");
            assert!(batch.push(cue));
        }

        let (finished_id, cues) = batch
            .finish(transaction_id)
            .expect("matching transaction ID")
            .expect("ten queued cues");
        assert_eq!(finished_id, transaction_id);
        assert_eq!(cues.len(), 10);
        assert!(!batch.is_active());
    }

    #[test]
    fn non_play_command_boundary_flushes_segment_but_keeps_transaction_open() {
        let transaction_id = Uuid::new_v4();
        let mut batch = LaunchBatchAccumulator::default();
        batch.begin(transaction_id);

        for _ in 0..3 {
            let cue = audio_play_request_from_command(play_command(Uuid::new_v4())).unwrap();
            assert!(batch.push(cue));
        }
        let (first_id, first_segment) = batch.take_pending().expect("first segment");
        assert_eq!(first_id, transaction_id);
        assert_eq!(first_segment.len(), 3);
        assert!(batch.is_active());

        for _ in 0..2 {
            let cue = audio_play_request_from_command(play_command(Uuid::new_v4())).unwrap();
            assert!(batch.push(cue));
        }
        let (last_id, last_segment) = batch
            .finish(transaction_id)
            .expect("matching transaction ID")
            .expect("second segment");
        assert_eq!(last_id, transaction_id);
        assert_eq!(last_segment.len(), 2);
        assert!(!batch.is_active());
    }
}
