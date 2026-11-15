//! Low-overhead session performance monitoring.
//!
//! System/process metrics are sampled by a dedicated background thread once
//! per second. GUI update timing is accumulated with atomics and never writes
//! to disk from the GUI or audio callback. Audio device APIs are not polled.

use crossbeam_channel::{bounded, Receiver, Sender};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const MAX_METRIC_SAMPLES: usize = 86_400; // Up to 24 hours at 1 Hz.
const GUI_BUCKET_UPPER_MS: [f64; 14] = [
    1.0,
    2.0,
    4.0,
    8.0,
    12.0,
    16.0,
    20.0,
    25.0,
    33.0,
    50.0,
    100.0,
    200.0,
    500.0,
    f64::INFINITY,
];

#[derive(Debug, Clone, Copy, Default)]
pub struct MetricSnapshot {
    pub current: Option<f64>,
    pub average: Option<f64>,
    pub p95: Option<f64>,
    pub maximum: Option<f64>,
    pub samples: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GuiUpdateSnapshot {
    pub frames: u64,
    pub average_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub maximum_ms: Option<f64>,
    pub over_33ms: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DispatchStatsSnapshot {
    pub batches: u64,
    pub events: u64,
    pub sent: u64,
    pub failed: u64,
    pub skipped: u64,
    pub average_queue_wait_us: Option<f64>,
    pub maximum_queue_wait_us: Option<u64>,
    pub average_worker_elapsed_us: Option<f64>,
    pub maximum_worker_elapsed_us: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct PerformanceSnapshot {
    pub session_elapsed_secs: u64,
    pub system_cpu: MetricSnapshot,
    pub app_cpu: MetricSnapshot,
    pub app_memory_mb: MetricSnapshot,
    pub system_memory_percent: MetricSnapshot,
    pub audio_worker_cpu: MetricSnapshot,
    pub audio_worker_memory_mb: MetricSnapshot,
    pub active_audio_workers: usize,
    pub worker_heartbeat_max_age_ms: Option<u64>,
    pub gui_update: GuiUpdateSnapshot,
    pub midi: DispatchStatsSnapshot,
    pub osc: DispatchStatsSnapshot,
    pub audio_worker_losses: u64,
    pub audio_worker_recoveries: u64,
    pub audio_worker_recovery_failures: u64,
}

#[derive(Default)]
struct MetricSeries {
    values: VecDeque<f64>,
    sum: f64,
    current: Option<f64>,
    maximum: Option<f64>,
}

impl MetricSeries {
    fn push(&mut self, value: f64) {
        if !value.is_finite() {
            return;
        }
        if self.values.len() == MAX_METRIC_SAMPLES {
            if let Some(oldest) = self.values.pop_front() {
                self.sum -= oldest;
            }
        }
        self.values.push_back(value);
        self.sum += value;
        self.current = Some(value);
        self.maximum = Some(self.maximum.map_or(value, |current| current.max(value)));
    }

    fn snapshot(&self) -> MetricSnapshot {
        if self.values.is_empty() {
            return MetricSnapshot::default();
        }
        let mut sorted: Vec<f64> = self.values.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);
        let p95_index = ((sorted.len() as f64 * 0.95).ceil() as usize)
            .saturating_sub(1)
            .min(sorted.len() - 1);
        MetricSnapshot {
            current: self.current,
            average: Some(self.sum / self.values.len() as f64),
            p95: Some(sorted[p95_index]),
            maximum: self.maximum,
            samples: self.values.len() as u64,
        }
    }
}

struct GuiUpdateAccumulator {
    frames: AtomicU64,
    total_ns: AtomicU64,
    maximum_ns: AtomicU64,
    over_33ms: AtomicU64,
    buckets: [AtomicU64; GUI_BUCKET_UPPER_MS.len()],
}

impl GuiUpdateAccumulator {
    fn new() -> Self {
        Self {
            frames: AtomicU64::new(0),
            total_ns: AtomicU64::new(0),
            maximum_ns: AtomicU64::new(0),
            over_33ms: AtomicU64::new(0),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    fn record(&self, elapsed: Duration) {
        let nanos = elapsed.as_nanos().min(u64::MAX as u128) as u64;
        let millis = nanos as f64 / 1_000_000.0;
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.total_ns.fetch_add(nanos, Ordering::Relaxed);
        self.maximum_ns.fetch_max(nanos, Ordering::Relaxed);
        if millis > 33.0 {
            self.over_33ms.fetch_add(1, Ordering::Relaxed);
        }
        let bucket = GUI_BUCKET_UPPER_MS
            .iter()
            .position(|upper| millis <= *upper)
            .unwrap_or(GUI_BUCKET_UPPER_MS.len() - 1);
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
    }

    fn take_interval(&self) -> GuiUpdateInterval {
        GuiUpdateInterval {
            frames: self.frames.swap(0, Ordering::Relaxed),
            total_ns: self.total_ns.swap(0, Ordering::Relaxed),
            maximum_ns: self.maximum_ns.swap(0, Ordering::Relaxed),
            over_33ms: self.over_33ms.swap(0, Ordering::Relaxed),
            buckets: std::array::from_fn(|index| self.buckets[index].swap(0, Ordering::Relaxed)),
        }
    }
}

struct GuiUpdateInterval {
    frames: u64,
    total_ns: u64,
    maximum_ns: u64,
    over_33ms: u64,
    buckets: [u64; GUI_BUCKET_UPPER_MS.len()],
}

#[derive(Default)]
struct GuiUpdateAggregate {
    frames: u64,
    total_ns: u128,
    maximum_ns: u64,
    over_33ms: u64,
    buckets: [u64; GUI_BUCKET_UPPER_MS.len()],
}

impl GuiUpdateAggregate {
    fn merge(&mut self, interval: GuiUpdateInterval) {
        self.frames = self.frames.saturating_add(interval.frames);
        self.total_ns = self.total_ns.saturating_add(interval.total_ns as u128);
        self.maximum_ns = self.maximum_ns.max(interval.maximum_ns);
        self.over_33ms = self.over_33ms.saturating_add(interval.over_33ms);
        for (total, added) in self.buckets.iter_mut().zip(interval.buckets) {
            *total = total.saturating_add(added);
        }
    }

    fn snapshot(&self) -> GuiUpdateSnapshot {
        if self.frames == 0 {
            return GuiUpdateSnapshot::default();
        }
        let target = ((self.frames as f64) * 0.95).ceil() as u64;
        let mut cumulative = 0_u64;
        let mut p95_ms = GUI_BUCKET_UPPER_MS[GUI_BUCKET_UPPER_MS.len() - 1];
        for (index, count) in self.buckets.iter().enumerate() {
            cumulative = cumulative.saturating_add(*count);
            if cumulative >= target {
                p95_ms = if index == GUI_BUCKET_UPPER_MS.len() - 1 {
                    self.maximum_ns as f64 / 1_000_000.0
                } else {
                    GUI_BUCKET_UPPER_MS[index]
                };
                break;
            }
        }
        GuiUpdateSnapshot {
            frames: self.frames,
            average_ms: Some(self.total_ns as f64 / self.frames as f64 / 1_000_000.0),
            p95_ms: Some(p95_ms),
            maximum_ms: Some(self.maximum_ns as f64 / 1_000_000.0),
            over_33ms: self.over_33ms,
        }
    }
}

struct DispatchCounters {
    batches: AtomicU64,
    events: AtomicU64,
    sent: AtomicU64,
    failed: AtomicU64,
    skipped: AtomicU64,
    queue_wait_total_us: AtomicU64,
    queue_wait_max_us: AtomicU64,
    worker_elapsed_total_us: AtomicU64,
    worker_elapsed_max_us: AtomicU64,
}

impl DispatchCounters {
    const fn new() -> Self {
        Self {
            batches: AtomicU64::new(0),
            events: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            queue_wait_total_us: AtomicU64::new(0),
            queue_wait_max_us: AtomicU64::new(0),
            worker_elapsed_total_us: AtomicU64::new(0),
            worker_elapsed_max_us: AtomicU64::new(0),
        }
    }

    fn record(
        &self,
        events: usize,
        sent: usize,
        failed: usize,
        skipped: usize,
        queue_wait_us: u128,
        worker_elapsed_us: u128,
    ) {
        let queue_wait_us = queue_wait_us.min(u64::MAX as u128) as u64;
        let worker_elapsed_us = worker_elapsed_us.min(u64::MAX as u128) as u64;
        self.batches.fetch_add(1, Ordering::Relaxed);
        self.events.fetch_add(events as u64, Ordering::Relaxed);
        self.sent.fetch_add(sent as u64, Ordering::Relaxed);
        self.failed.fetch_add(failed as u64, Ordering::Relaxed);
        self.skipped.fetch_add(skipped as u64, Ordering::Relaxed);
        self.queue_wait_total_us
            .fetch_add(queue_wait_us, Ordering::Relaxed);
        self.queue_wait_max_us
            .fetch_max(queue_wait_us, Ordering::Relaxed);
        self.worker_elapsed_total_us
            .fetch_add(worker_elapsed_us, Ordering::Relaxed);
        self.worker_elapsed_max_us
            .fetch_max(worker_elapsed_us, Ordering::Relaxed);
    }

    fn snapshot(&self) -> DispatchStatsSnapshot {
        let batches = self.batches.load(Ordering::Relaxed);
        let queue_total = self.queue_wait_total_us.load(Ordering::Relaxed);
        let worker_total = self.worker_elapsed_total_us.load(Ordering::Relaxed);
        DispatchStatsSnapshot {
            batches,
            events: self.events.load(Ordering::Relaxed),
            sent: self.sent.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            skipped: self.skipped.load(Ordering::Relaxed),
            average_queue_wait_us: (batches > 0).then_some(queue_total as f64 / batches as f64),
            maximum_queue_wait_us: (batches > 0)
                .then_some(self.queue_wait_max_us.load(Ordering::Relaxed)),
            average_worker_elapsed_us: (batches > 0)
                .then_some(worker_total as f64 / batches as f64),
            maximum_worker_elapsed_us: (batches > 0)
                .then_some(self.worker_elapsed_max_us.load(Ordering::Relaxed)),
        }
    }
}

static MIDI_DISPATCH_COUNTERS: DispatchCounters = DispatchCounters::new();
static OSC_DISPATCH_COUNTERS: DispatchCounters = DispatchCounters::new();

pub fn record_midi_batch(
    events: usize,
    sent: usize,
    failed: usize,
    skipped: usize,
    queue_wait_us: u128,
    worker_elapsed_us: u128,
) {
    MIDI_DISPATCH_COUNTERS.record(
        events,
        sent,
        failed,
        skipped,
        queue_wait_us,
        worker_elapsed_us,
    );
}

pub fn record_osc_batch(
    events: usize,
    sent: usize,
    failed: usize,
    queue_wait_us: u128,
    worker_elapsed_us: u128,
) {
    OSC_DISPATCH_COUNTERS.record(events, sent, failed, 0, queue_wait_us, worker_elapsed_us);
}

#[derive(Default)]
struct AudioWorkerCounters {
    losses: AtomicU64,
    recoveries: AtomicU64,
    recovery_failures: AtomicU64,
}

#[derive(Default)]
struct WorkerProcessState {
    pids: Vec<u32>,
    heartbeat_max_age_ms: Option<u64>,
}

enum SamplerCommand {
    Shutdown,
}

pub struct PerformanceMonitor {
    snapshot: Arc<RwLock<PerformanceSnapshot>>,
    gui_updates: Arc<GuiUpdateAccumulator>,
    worker_state: Arc<RwLock<WorkerProcessState>>,
    audio_worker_counters: Arc<AudioWorkerCounters>,
    shutdown_tx: Sender<SamplerCommand>,
    sampler_thread: Option<JoinHandle<()>>,
}

impl PerformanceMonitor {
    pub fn new() -> Self {
        let snapshot = Arc::new(RwLock::new(PerformanceSnapshot::default()));
        let gui_updates = Arc::new(GuiUpdateAccumulator::new());
        let worker_state = Arc::new(RwLock::new(WorkerProcessState::default()));
        let audio_worker_counters = Arc::new(AudioWorkerCounters::default());
        let (shutdown_tx, shutdown_rx) = bounded(1);

        let thread_snapshot = snapshot.clone();
        let thread_gui_updates = gui_updates.clone();
        let thread_worker_state = worker_state.clone();
        let thread_audio_counters = audio_worker_counters.clone();
        let sampler_thread = thread::Builder::new()
            .name("tasq-performance-monitor".to_string())
            .spawn(move || {
                run_performance_sampler(
                    thread_snapshot,
                    thread_gui_updates,
                    thread_worker_state,
                    thread_audio_counters,
                    shutdown_rx,
                )
            })
            .ok();

        if sampler_thread.is_none() {
            eprintln!("Could not start TaSQ performance sampler thread; performance metrics will be unavailable");
        }

        Self {
            snapshot,
            gui_updates,
            worker_state,
            audio_worker_counters,
            shutdown_tx,
            sampler_thread,
        }
    }

    /// Record the duration of the current egui update. This is an atomic-only
    /// operation; it performs no allocation, locking, file I/O or driver calls.
    pub fn record_gui_update(&self, elapsed: Duration) {
        self.gui_updates.record(elapsed);
    }

    pub fn set_audio_workers(&self, pids: Vec<u32>, heartbeat_max_age_ms: Option<u64>) {
        if let Ok(mut state) = self.worker_state.write() {
            state.pids = pids.into_iter().filter(|pid| *pid > 0).collect();
            state.heartbeat_max_age_ms = heartbeat_max_age_ms;
        }
    }

    pub fn record_audio_worker_lost(&self) {
        self.audio_worker_counters
            .losses
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_audio_worker_recovered(&self) {
        self.audio_worker_counters
            .recoveries
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_audio_worker_recovery_failed(&self) {
        self.audio_worker_counters
            .recovery_failures
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> PerformanceSnapshot {
        self.snapshot
            .read()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_default()
    }

    pub fn session_summary(&self) -> String {
        format_performance_summary(&self.snapshot())
    }
}

impl Default for PerformanceMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PerformanceMonitor {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(SamplerCommand::Shutdown);
        if let Some(thread) = self.sampler_thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_performance_sampler(
    shared_snapshot: Arc<RwLock<PerformanceSnapshot>>,
    gui_updates: Arc<GuiUpdateAccumulator>,
    worker_state: Arc<RwLock<WorkerProcessState>>,
    audio_worker_counters: Arc<AudioWorkerCounters>,
    shutdown_rx: Receiver<SamplerCommand>,
) {
    let session_started_at = Instant::now();
    let mut last_sample_at = Instant::now();
    let mut system_cpu = SystemCpuMonitor::new();
    let mut app_cpu_series = MetricSeries::default();
    let mut system_cpu_series = MetricSeries::default();
    let mut app_memory_series = MetricSeries::default();
    let mut system_memory_series = MetricSeries::default();
    let mut worker_cpu_series = MetricSeries::default();
    let mut worker_memory_series = MetricSeries::default();
    let mut gui_aggregate = GuiUpdateAggregate::default();
    let mut previous_app_cpu = read_process_metrics(None).map(|metrics| metrics.cpu_time_100ns);
    let mut previous_worker_cpu: HashMap<u32, u64> = HashMap::new();
    let mut previous_worker_pids: Vec<u32> = Vec::new();

    loop {
        match shutdown_rx.recv_timeout(SAMPLE_INTERVAL) {
            Ok(SamplerCommand::Shutdown)
            | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                gui_aggregate.merge(gui_updates.take_interval());
                if let Ok(mut snapshot) = shared_snapshot.write() {
                    snapshot.gui_update = gui_aggregate.snapshot();
                }
                break;
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }

        let now = Instant::now();
        let elapsed = now.saturating_duration_since(last_sample_at);
        if elapsed.is_zero() {
            continue;
        }
        last_sample_at = now;

        system_cpu.update();
        if let Some(value) = system_cpu.usage_percent() {
            system_cpu_series.push(value as f64);
        }

        let app_metrics = read_process_metrics(None);
        let app_cpu = app_metrics.as_ref().and_then(|current| {
            let previous = previous_app_cpu.replace(current.cpu_time_100ns)?;
            let delta = current.cpu_time_100ns.saturating_sub(previous);
            calculate_process_cpu_percent(delta, elapsed)
        });
        if let Some(value) = app_cpu {
            app_cpu_series.push(value);
        }
        if let Some(metrics) = &app_metrics {
            app_memory_series.push(metrics.resident_memory_bytes as f64 / (1024.0 * 1024.0));
        }

        let worker_state_snapshot = worker_state
            .read()
            .map(|state| WorkerProcessState {
                pids: state.pids.clone(),
                heartbeat_max_age_ms: state.heartbeat_max_age_ms,
            })
            .unwrap_or_default();
        let mut current_worker_cpu = HashMap::new();
        let mut worker_memory_bytes = 0_u64;
        for pid in &worker_state_snapshot.pids {
            if let Some(metrics) = read_process_metrics(Some(*pid)) {
                current_worker_cpu.insert(*pid, metrics.cpu_time_100ns);
                worker_memory_bytes =
                    worker_memory_bytes.saturating_add(metrics.resident_memory_bytes);
            }
        }

        let worker_cpu = if worker_state_snapshot.pids == previous_worker_pids
            && !current_worker_cpu.is_empty()
            && current_worker_cpu.len() == previous_worker_cpu.len()
        {
            let delta = current_worker_cpu
                .iter()
                .fold(0_u64, |total, (pid, current)| {
                    total.saturating_add(
                        current.saturating_sub(*previous_worker_cpu.get(pid).unwrap_or(current)),
                    )
                });
            calculate_process_cpu_percent(delta, elapsed)
        } else {
            None
        };
        previous_worker_pids = worker_state_snapshot.pids.clone();
        previous_worker_cpu = current_worker_cpu;

        if let Some(value) = worker_cpu {
            worker_cpu_series.push(value);
        }
        if !worker_state_snapshot.pids.is_empty() {
            worker_memory_series.push(worker_memory_bytes as f64 / (1024.0 * 1024.0));
        }

        if let Some((used_percent, _total_bytes)) = read_system_memory() {
            system_memory_series.push(used_percent);
        }

        gui_aggregate.merge(gui_updates.take_interval());

        let snapshot = PerformanceSnapshot {
            session_elapsed_secs: session_started_at.elapsed().as_secs(),
            system_cpu: system_cpu_series.snapshot(),
            app_cpu: app_cpu_series.snapshot(),
            app_memory_mb: app_memory_series.snapshot(),
            system_memory_percent: system_memory_series.snapshot(),
            audio_worker_cpu: worker_cpu_series.snapshot(),
            audio_worker_memory_mb: worker_memory_series.snapshot(),
            active_audio_workers: worker_state_snapshot.pids.len(),
            worker_heartbeat_max_age_ms: worker_state_snapshot.heartbeat_max_age_ms,
            gui_update: gui_aggregate.snapshot(),
            midi: MIDI_DISPATCH_COUNTERS.snapshot(),
            osc: OSC_DISPATCH_COUNTERS.snapshot(),
            audio_worker_losses: audio_worker_counters.losses.load(Ordering::Relaxed),
            audio_worker_recoveries: audio_worker_counters.recoveries.load(Ordering::Relaxed),
            audio_worker_recovery_failures: audio_worker_counters
                .recovery_failures
                .load(Ordering::Relaxed),
        };

        if let Ok(mut shared) = shared_snapshot.write() {
            *shared = snapshot;
        }
    }
}

fn calculate_process_cpu_percent(cpu_delta_100ns: u64, elapsed: Duration) -> Option<f64> {
    let elapsed_100ns = elapsed.as_nanos() as f64 / 100.0;
    if elapsed_100ns <= 0.0 {
        return None;
    }
    let logical_cores = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .max(1) as f64;
    Some(((cpu_delta_100ns as f64 / elapsed_100ns) / logical_cores * 100.0).clamp(0.0, 100.0))
}

fn format_metric(metric: MetricSnapshot, unit: &str) -> String {
    match (metric.average, metric.p95, metric.maximum) {
        (Some(average), Some(p95), Some(maximum)) => {
            format!("avg={average:.2}{unit}, p95={p95:.2}{unit}, max={maximum:.2}{unit}, retained_samples={}", metric.samples)
        }
        _ => "no samples".to_string(),
    }
}

pub fn format_performance_summary(snapshot: &PerformanceSnapshot) -> String {
    let gui = snapshot.gui_update;
    let mut output = String::new();
    output.push_str("Theatre Show Cue Performance Monitor\n");
    output.push_str(&format!(
        "Session duration: {}s\n",
        snapshot.session_elapsed_secs
    ));
    output.push_str(&format!(
        "System CPU: {}\n",
        format_metric(snapshot.system_cpu, "%")
    ));
    output.push_str(&format!(
        "TaSQ process CPU: {}\n",
        format_metric(snapshot.app_cpu, "%")
    ));
    output.push_str(&format!(
        "TaSQ resident memory: {}\n",
        format_metric(snapshot.app_memory_mb, " MB")
    ));
    output.push_str(&format!(
        "System memory load: {}\n",
        format_metric(snapshot.system_memory_percent, "%")
    ));
    output.push_str(&format!(
        "Audio Worker CPU (aggregate): {}\n",
        format_metric(snapshot.audio_worker_cpu, "%")
    ));
    output.push_str(&format!(
        "Audio Worker resident memory (aggregate): {}\n",
        format_metric(snapshot.audio_worker_memory_mb, " MB")
    ));
    output.push_str(&format!(
        "Active Audio Workers: {}\n",
        snapshot.active_audio_workers
    ));
    output.push_str(&format!(
        "Maximum Audio Worker heartbeat age: {} ms\n",
        snapshot
            .worker_heartbeat_max_age_ms
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unavailable".to_string())
    ));
    output.push_str(&format!(
        "GUI update: frames={}, avg={:?} ms, p95={:?} ms, max={:?} ms, >33ms={}\n",
        gui.frames, gui.average_ms, gui.p95_ms, gui.maximum_ms, gui.over_33ms
    ));
    append_dispatch_summary(&mut output, "MIDI", snapshot.midi);
    append_dispatch_summary(&mut output, "OSC", snapshot.osc);
    output.push_str(&format!(
        "Audio Worker endpoint losses={}, recoveries={}, recovery failures={}\n",
        snapshot.audio_worker_losses,
        snapshot.audio_worker_recoveries,
        snapshot.audio_worker_recovery_failures
    ));
    output.push_str(
        "Note: the current audio backend API does not expose a reliable hardware underrun counter. Endpoint/Worker failures are counted separately. MIDI sent counts mean accepted by the MIDI backend; OSC sent counts mean accepted by the local UDP stack, not acknowledged by the receiver. Process CPU percentages are normalized to total logical CPU capacity.\n",
    );
    output
}

fn append_dispatch_summary(output: &mut String, label: &str, stats: DispatchStatsSnapshot) {
    output.push_str(&format!(
        "{label}: batches={}, events={}, sent={}, failed={}, skipped={}, avg_queue_wait={:?} us, max_queue_wait={:?} us, avg_worker_elapsed={:?} us, max_worker_elapsed={:?} us\n",
        stats.batches,
        stats.events,
        stats.sent,
        stats.failed,
        stats.skipped,
        stats.average_queue_wait_us,
        stats.maximum_queue_wait_us,
        stats.average_worker_elapsed_us,
        stats.maximum_worker_elapsed_us
    ));
}

#[derive(Debug)]
struct ProcessMetrics {
    cpu_time_100ns: u64,
    resident_memory_bytes: u64,
}

#[cfg(target_os = "windows")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileTime {
    low_date_time: u32,
    high_date_time: u32,
}

#[cfg(target_os = "windows")]
impl FileTime {
    fn as_u64(self) -> u64 {
        ((self.high_date_time as u64) << 32) | self.low_date_time as u64
    }
}

#[cfg(target_os = "windows")]
#[repr(C)]
struct ProcessMemoryCountersEx {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_usage: usize,
}

#[cfg(target_os = "windows")]
#[repr(C)]
struct MemoryStatusEx {
    length: u32,
    memory_load: u32,
    total_physical: u64,
    available_physical: u64,
    total_page_file: u64,
    available_page_file: u64,
    total_virtual: u64,
    available_virtual: u64,
    available_extended_virtual: u64,
}

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> *mut std::ffi::c_void;
    fn OpenProcess(access: u32, inherit_handle: i32, process_id: u32) -> *mut std::ffi::c_void;
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    fn GetProcessTimes(
        process: *mut std::ffi::c_void,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
    fn K32GetProcessMemoryInfo(
        process: *mut std::ffi::c_void,
        counters: *mut ProcessMemoryCountersEx,
        size: u32,
    ) -> i32;
    fn GlobalMemoryStatusEx(status: *mut MemoryStatusEx) -> i32;
}

#[cfg(target_os = "windows")]
fn read_process_metrics(pid: Option<u32>) -> Option<ProcessMetrics> {
    unsafe {
        let (process, should_close) = match pid {
            Some(pid) => {
                // PROCESS_QUERY_INFORMATION | PROCESS_VM_READ
                let handle = OpenProcess(0x0400 | 0x0010, 0, pid);
                if handle.is_null() {
                    return None;
                }
                (handle, true)
            }
            None => (GetCurrentProcess(), false),
        };

        let mut creation = FileTime::default();
        let mut exit = FileTime::default();
        let mut kernel = FileTime::default();
        let mut user = FileTime::default();
        let times_ok =
            GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) != 0;

        let mut memory = ProcessMemoryCountersEx {
            cb: std::mem::size_of::<ProcessMemoryCountersEx>() as u32,
            page_fault_count: 0,
            peak_working_set_size: 0,
            working_set_size: 0,
            quota_peak_paged_pool_usage: 0,
            quota_paged_pool_usage: 0,
            quota_peak_non_paged_pool_usage: 0,
            quota_non_paged_pool_usage: 0,
            pagefile_usage: 0,
            peak_pagefile_usage: 0,
            private_usage: 0,
        };
        let memory_ok = K32GetProcessMemoryInfo(
            process,
            &mut memory,
            std::mem::size_of::<ProcessMemoryCountersEx>() as u32,
        ) != 0;

        if should_close {
            let _ = CloseHandle(process);
        }
        if !times_ok {
            return None;
        }

        Some(ProcessMetrics {
            cpu_time_100ns: kernel.as_u64().saturating_add(user.as_u64()),
            resident_memory_bytes: if memory_ok {
                memory.working_set_size as u64
            } else {
                0
            },
        })
    }
}

#[cfg(target_os = "windows")]
fn read_system_memory() -> Option<(f64, u64)> {
    unsafe {
        let mut status = MemoryStatusEx {
            length: std::mem::size_of::<MemoryStatusEx>() as u32,
            memory_load: 0,
            total_physical: 0,
            available_physical: 0,
            total_page_file: 0,
            available_page_file: 0,
            total_virtual: 0,
            available_virtual: 0,
            available_extended_virtual: 0,
        };
        if GlobalMemoryStatusEx(&mut status) == 0 || status.total_physical == 0 {
            return None;
        }
        Some((
            (status
                .total_physical
                .saturating_sub(status.available_physical)) as f64
                / status.total_physical as f64
                * 100.0,
            status.total_physical,
        ))
    }
}

#[cfg(target_os = "linux")]
fn read_process_metrics(pid: Option<u32>) -> Option<ProcessMetrics> {
    let pid = pid.unwrap_or_else(std::process::id);
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let closing_paren = stat.rfind(')')?;
    let fields: Vec<&str> = stat.get(closing_paren + 1..)?.split_whitespace().collect();
    let user_ticks = fields.get(11)?.parse::<u64>().ok()?;
    let system_ticks = fields.get(12)?.parse::<u64>().ok();
    let ticks_per_second = unsafe { sysconf(2) }.max(1) as u64;
    let cpu_ticks = user_ticks.saturating_add(system_ticks);
    let cpu_time_100ns = cpu_ticks.saturating_mul(10_000_000) / ticks_per_second;

    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let resident_kb = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);

    Some(ProcessMetrics {
        cpu_time_100ns,
        resident_memory_bytes: resident_kb.saturating_mul(1024),
    })
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn sysconf(name: i32) -> i64;
}

#[cfg(target_os = "linux")]
fn read_system_memory() -> Option<(f64, u64)> {
    let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
    let value_kb = |key: &str| {
        contents
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
    };
    let total = value_kb("MemTotal:")?.saturating_mul(1024);
    let available = value_kb("MemAvailable:")?.saturating_mul(1024);
    if total == 0 {
        return None;
    }
    Some((
        (total.saturating_sub(available)) as f64 / total as f64 * 100.0,
        total,
    ))
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn read_process_metrics(_pid: Option<u32>) -> Option<ProcessMetrics> {
    None
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn read_system_memory() -> Option<(f64, u64)> {
    None
}

/// Lightweight system-wide CPU sampling. Retained as the source for the
/// transport's system CPU indicator; the Performance Monitor owns this on its
/// sampler thread.
#[derive(Debug)]
pub struct SystemCpuMonitor {
    last_sample_at: Instant,
    previous_times: Option<(u64, u64)>,
    usage_percent: Option<f32>,
}

impl SystemCpuMonitor {
    pub fn new() -> Self {
        Self {
            last_sample_at: Instant::now(),
            previous_times: read_cpu_times(),
            usage_percent: None,
        }
    }

    pub fn update(&mut self) {
        let now = Instant::now();
        if now.saturating_duration_since(self.last_sample_at) < Duration::from_secs(1) {
            return;
        }
        self.last_sample_at = now;

        let Some(current) = read_cpu_times() else {
            self.usage_percent = None;
            self.previous_times = None;
            return;
        };

        if let Some(previous) = self.previous_times {
            let idle_delta = current.0.saturating_sub(previous.0);
            let total_delta = current.1.saturating_sub(previous.1);
            if total_delta > 0 {
                let busy_delta = total_delta.saturating_sub(idle_delta);
                self.usage_percent =
                    Some((busy_delta as f64 / total_delta as f64 * 100.0).clamp(0.0, 100.0) as f32);
            }
        }
        self.previous_times = Some(current);
    }

    pub fn usage_percent(&self) -> Option<f32> {
        self.usage_percent
    }
}

#[cfg(target_os = "windows")]
fn read_cpu_times() -> Option<(u64, u64)> {
    let mut idle = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    let success = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
    if success == 0 {
        return None;
    }
    let idle = idle.as_u64();
    let total = kernel.as_u64().saturating_add(user.as_u64());
    Some((idle, total))
}

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
extern "system" {
    fn GetSystemTimes(
        idle_time: *mut FileTime,
        kernel_time: *mut FileTime,
        user_time: *mut FileTime,
    ) -> i32;
}

#[cfg(target_os = "linux")]
fn read_cpu_times() -> Option<(u64, u64)> {
    let contents = std::fs::read_to_string("/proc/stat").ok()?;
    let cpu_line = contents.lines().find(|line| line.starts_with("cpu "))?;
    let values: Vec<u64> = cpu_line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|value| value.parse::<u64>())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if values.len() < 4 {
        return None;
    }
    let idle = values[3].saturating_add(*values.get(4).unwrap_or(&0));
    let total = values.iter().copied().sum();
    Some((idle, total))
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn read_cpu_times() -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::{calculate_process_cpu_percent, GuiUpdateAggregate, GuiUpdateInterval};
    use std::time::Duration;

    #[test]
    fn process_cpu_is_normalized_by_logical_core_count_and_clamped() {
        let one_core_second = 10_000_000_u64;
        let usage = calculate_process_cpu_percent(one_core_second, Duration::from_secs(1))
            .expect("CPU metric");
        let cores = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1);
        assert!((usage - (100.0 / cores as f64)).abs() < 0.001);
        assert_eq!(
            calculate_process_cpu_percent(u64::MAX, Duration::from_millis(1)),
            Some(100.0)
        );
    }

    #[test]
    fn gui_update_percentile_uses_histogram_and_tracks_slow_frames() {
        let mut aggregate = GuiUpdateAggregate::default();
        let mut buckets = [0_u64; super::GUI_BUCKET_UPPER_MS.len()];
        buckets[7] = 19; // <=25 ms
        buckets[8] = 1; // <=33 ms
        aggregate.merge(GuiUpdateInterval {
            frames: 20,
            total_ns: 19 * 20_000_000 + 30_000_000,
            maximum_ns: 30_000_000,
            over_33ms: 0,
            buckets,
        });
        let snapshot = aggregate.snapshot();
        assert_eq!(snapshot.frames, 20);
        assert_eq!(snapshot.p95_ms, Some(25.0));
        assert_eq!(snapshot.maximum_ms, Some(30.0));
    }
}
