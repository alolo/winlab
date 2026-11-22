// src/audio/waveform.rs — анализ аудиофайлов и кэширование waveform

use crate::project::asset_manager::AssetManager;
use crossbeam_channel::{unbounded, Receiver, Sender};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rodio::Source;
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::UNIX_EPOCH;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

const DEFAULT_WAVEFORM_BUCKETS: usize = 4096;
const MAX_AUDIO_ANALYSIS_CACHE_ENTRIES: usize = 512;
const MAX_PERSISTENT_CACHE_ENTRIES: usize = 1024;
const DISK_CACHE_MAGIC: &[u8; 8] = b"TSQWF001";
static DISK_CACHE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static DISK_CACHE_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Runtime audio metadata shared by every Cue that references the same file.
#[derive(Debug, Clone)]
pub struct AudioFileAnalysis {
    /// Interleaved [min, max] values used by the waveform editor.
    /// Arc allows hundreds of Cues to share one waveform allocation.
    pub waveform: Arc<[f32]>,
    pub duration_secs: f64,
    pub channels: usize,
}

impl Default for AudioFileAnalysis {
    fn default() -> Self {
        Self {
            waveform: Arc::from(Vec::<f32>::new()),
            duration_secs: 0.0,
            channels: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AudioAnalysisCacheKey {
    canonical_path: String,
    file_size: u64,
    modified_nanos: Option<u128>,
    buckets: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AudioAnalysisCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub decodes: u64,
    pub disk_hits: u64,
    pub disk_misses: u64,
    pub disk_writes: u64,
    pub evictions: u64,
    pub entries: usize,
}

struct AudioAnalysisCache {
    entries: HashMap<AudioAnalysisCacheKey, Arc<AudioFileAnalysis>>,
    recency: VecDeque<AudioAnalysisCacheKey>,
    hits: u64,
    misses: u64,
    decodes: u64,
    disk_hits: u64,
    disk_misses: u64,
    disk_writes: u64,
    evictions: u64,
    capacity: usize,
}

impl Default for AudioAnalysisCache {
    fn default() -> Self {
        Self::with_capacity(MAX_AUDIO_ANALYSIS_CACHE_ENTRIES)
    }
}

impl AudioAnalysisCache {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            recency: VecDeque::new(),
            hits: 0,
            misses: 0,
            decodes: 0,
            disk_hits: 0,
            disk_misses: 0,
            disk_writes: 0,
            evictions: 0,
            capacity: capacity.max(1),
        }
    }

    fn get(&mut self, key: &AudioAnalysisCacheKey) -> Option<Arc<AudioFileAnalysis>> {
        let analysis = self.entries.get(key).cloned();
        if analysis.is_some() {
            self.hits = self.hits.saturating_add(1);
            self.touch(key);
        } else {
            self.misses = self.misses.saturating_add(1);
        }
        analysis
    }

    fn insert(
        &mut self,
        key: AudioAnalysisCacheKey,
        analysis: Arc<AudioFileAnalysis>,
        decoded: bool,
    ) -> Arc<AudioFileAnalysis> {
        if decoded {
            self.decodes = self.decodes.saturating_add(1);
        }
        // A changed file must never keep serving an older waveform. Remove
        // previous fingerprints for this path, while retaining other bucket
        // resolutions for the current file version.
        let stale_keys: Vec<AudioAnalysisCacheKey> = self
            .entries
            .keys()
            .filter(|existing| {
                existing.canonical_path == key.canonical_path
                    && (existing.file_size != key.file_size
                        || existing.modified_nanos != key.modified_nanos)
            })
            .cloned()
            .collect();

        for stale_key in stale_keys {
            self.entries.remove(&stale_key);
            self.recency.retain(|entry| entry != &stale_key);
        }

        // Another thread may have completed the same decode while this one
        // was working. Prefer the already-published shared result.
        if let Some(existing) = self.entries.get(&key).cloned() {
            self.touch(&key);
            return existing;
        }

        self.entries.insert(key.clone(), analysis.clone());
        self.recency.retain(|entry| entry != &key);
        self.recency.push_back(key);

        while self.entries.len() > self.capacity {
            let Some(oldest) = self.recency.pop_front() else {
                break;
            };
            if self.entries.remove(&oldest).is_some() {
                self.evictions = self.evictions.saturating_add(1);
            }
        }

        analysis
    }

    fn touch(&mut self, key: &AudioAnalysisCacheKey) {
        self.recency.retain(|entry| entry != key);
        self.recency.push_back(key.clone());
    }

    fn record_disk_hit(&mut self) {
        self.disk_hits = self.disk_hits.saturating_add(1);
    }

    fn record_disk_miss(&mut self) {
        self.disk_misses = self.disk_misses.saturating_add(1);
    }

    fn record_disk_write(&mut self) {
        self.disk_writes = self.disk_writes.saturating_add(1);
    }

    fn stats(&self) -> AudioAnalysisCacheStats {
        AudioAnalysisCacheStats {
            hits: self.hits,
            misses: self.misses,
            decodes: self.decodes,
            disk_hits: self.disk_hits,
            disk_misses: self.disk_misses,
            disk_writes: self.disk_writes,
            evictions: self.evictions,
            entries: self.entries.len(),
        }
    }
}

static AUDIO_ANALYSIS_CACHE: Lazy<Mutex<AudioAnalysisCache>> =
    Lazy::new(|| Mutex::new(AudioAnalysisCache::default()));

/// Returns a snapshot of process-wide audio-analysis cache counters.
pub fn audio_analysis_cache_stats() -> AudioAnalysisCacheStats {
    AUDIO_ANALYSIS_CACHE.lock().stats()
}

/// Decode and analyze an audio file once, then reuse its waveform, duration,
/// and channel count for every Cue that references the same file fingerprint.
///
/// The cache key includes the canonical path, file size, modification time,
/// and waveform resolution. Replacing or editing a file invalidates its old
/// cached analysis automatically. The cache is bounded to avoid unbounded
/// memory growth while working with many different projects.
pub fn analyze_audio_file(path: &Path, buckets: usize) -> Arc<AudioFileAnalysis> {
    if path.as_os_str().is_empty() || buckets == 0 {
        return Arc::new(AudioFileAnalysis::default());
    }

    let Some(key) = make_cache_key(path, buckets) else {
        // Missing or inaccessible files are not cached; they may become
        // available later, and the decoder below will provide the usual error.
        return Arc::new(decode_audio_file(path, buckets));
    };

    if let Some(cached) = AUDIO_ANALYSIS_CACHE.lock().get(&key) {
        trace!(
            "Audio analysis memory-cache hit: {:?}, buckets={}",
            path,
            buckets
        );
        return cached;
    }

    if let Some(cached) = read_persistent_cache(&key) {
        AUDIO_ANALYSIS_CACHE.lock().record_disk_hit();
        trace!(
            "Audio analysis disk-cache hit: {:?}, buckets={}",
            path,
            buckets
        );
        return AUDIO_ANALYSIS_CACHE
            .lock()
            .insert(key, Arc::new(cached), false);
    }

    AUDIO_ANALYSIS_CACHE.lock().record_disk_miss();
    let analysis = Arc::new(decode_audio_file(path, buckets));
    if analysis.waveform.is_empty() || analysis.channels == 0 {
        return analysis;
    }

    match write_persistent_cache(&key, &analysis) {
        Ok(true) => AUDIO_ANALYSIS_CACHE.lock().record_disk_write(),
        Ok(false) => {}
        Err(error) => warn!("Could not persist waveform cache for {:?}: {error}", path),
    }

    AUDIO_ANALYSIS_CACHE.lock().insert(key, analysis, true)
}

/// Backwards-compatible waveform-only helper.
#[allow(dead_code)]
pub fn generate_waveform(path: &Path, buckets: usize) -> Vec<f32> {
    analyze_audio_file(path, buckets).waveform.to_vec()
}

/// Backwards-compatible duration helper. App code should prefer
/// `analyze_audio_file` so duration, channels, and waveform share one decode.
#[allow(dead_code)]
pub fn get_duration(path: &Path) -> f64 {
    analyze_audio_file(path, DEFAULT_WAVEFORM_BUCKETS).duration_secs
}

/// Backwards-compatible channel-count helper. App code should prefer
/// `analyze_audio_file` so duration, channels, and waveform share one decode.
#[allow(dead_code)]
pub fn get_channel_count(path: &Path) -> usize {
    analyze_audio_file(path, DEFAULT_WAVEFORM_BUCKETS).channels
}

fn make_cache_key(path: &Path, buckets: usize) -> Option<AudioAnalysisCacheKey> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }

    let normalized_path: PathBuf = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut canonical_path = normalized_path.to_string_lossy().into_owned();

    #[cfg(windows)]
    {
        canonical_path = canonical_path.to_lowercase();
    }

    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());

    Some(AudioAnalysisCacheKey {
        canonical_path,
        file_size: metadata.len(),
        modified_nanos,
        buckets,
    })
}

fn persistent_cache_directory() -> Option<PathBuf> {
    dirs::data_local_dir()
        .or_else(dirs::cache_dir)
        .map(|directory| directory.join("TheatreShowCue").join("waveform_cache_v1"))
}

fn persistent_cache_path(key: &AudioAnalysisCacheKey) -> Option<PathBuf> {
    let directory = persistent_cache_directory()?;
    let hash = stable_cache_key_hash(key);
    Some(directory.join(format!("{hash:016x}.tsqwf")))
}

/// Stable FNV-1a key hashing keeps cache filenames consistent between runs.
/// The complete key is also stored in the cache file and validated on read.
fn stable_cache_key_hash(key: &AudioAnalysisCacheKey) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    };
    feed(key.canonical_path.as_bytes());
    feed(&key.file_size.to_le_bytes());
    match key.modified_nanos {
        Some(value) => {
            feed(&[1]);
            feed(&value.to_le_bytes());
        }
        None => feed(&[0]),
    }
    feed(&(key.buckets as u64).to_le_bytes());
    hash
}

fn read_persistent_cache(key: &AudioAnalysisCacheKey) -> Option<AudioFileAnalysis> {
    let cache_path = persistent_cache_path(key)?;
    let file = File::open(cache_path).ok()?;
    let mut reader = BufReader::new(file);

    let mut magic = [0_u8; 8];
    reader.read_exact(&mut magic).ok()?;
    if &magic != DISK_CACHE_MAGIC {
        return None;
    }

    let path_len = read_u32(&mut reader).ok()? as usize;
    if path_len == 0 || path_len > 32_768 {
        return None;
    }
    let mut stored_path = vec![0_u8; path_len];
    reader.read_exact(&mut stored_path).ok()?;
    if stored_path.as_slice() != key.canonical_path.as_bytes() {
        return None;
    }

    let file_size = read_u64(&mut reader).ok()?;
    let modified_flag = read_u8(&mut reader).ok()?;
    let modified_nanos = match modified_flag {
        0 => None,
        1 => Some(read_u128(&mut reader).ok()?),
        _ => return None,
    };
    let buckets = read_u32(&mut reader).ok()? as usize;
    let channels = read_u32(&mut reader).ok()? as usize;
    let duration_secs = read_f64(&mut reader).ok()?;
    let waveform_len = read_u64(&mut reader).ok()? as usize;

    if file_size != key.file_size
        || modified_nanos != key.modified_nanos
        || buckets != key.buckets
        || channels == 0
        || channels > 256
        || !duration_secs.is_finite()
        || duration_secs <= 0.0
        || waveform_len != buckets.checked_mul(2)?
        || waveform_len > 131_072
    {
        return None;
    }

    let mut waveform = Vec::with_capacity(waveform_len);
    for _ in 0..waveform_len {
        let value = read_f32(&mut reader).ok()?;
        if !value.is_finite() || !(-1.01..=1.01).contains(&value) {
            return None;
        }
        waveform.push(value);
    }

    Some(AudioFileAnalysis {
        waveform: Arc::from(waveform),
        duration_secs,
        channels,
    })
}

fn write_persistent_cache(
    key: &AudioAnalysisCacheKey,
    analysis: &AudioFileAnalysis,
) -> std::io::Result<bool> {
    let Some(cache_path) = persistent_cache_path(key) else {
        return Ok(false);
    };
    let Some(parent) = cache_path.parent() else {
        return Ok(false);
    };
    fs::create_dir_all(parent)?;

    let temp_id = DISK_CACHE_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_path = cache_path.with_extension(format!("tmp-{}-{temp_id}", std::process::id()));
    let file = File::create(&temp_path)?;
    let mut writer = BufWriter::new(file);

    writer.write_all(DISK_CACHE_MAGIC)?;
    write_u32(&mut writer, key.canonical_path.len() as u32)?;
    writer.write_all(key.canonical_path.as_bytes())?;
    write_u64(&mut writer, key.file_size)?;
    match key.modified_nanos {
        Some(value) => {
            write_u8(&mut writer, 1)?;
            write_u128(&mut writer, value)?;
        }
        None => write_u8(&mut writer, 0)?,
    }
    write_u32(&mut writer, key.buckets as u32)?;
    write_u32(&mut writer, analysis.channels as u32)?;
    write_f64(&mut writer, analysis.duration_secs)?;
    write_u64(&mut writer, analysis.waveform.len() as u64)?;
    for value in analysis.waveform.iter() {
        write_f32(&mut writer, *value)?;
    }
    writer.flush()?;
    drop(writer);

    // Another worker may have written the same key while this worker decoded.
    // Keep a valid completed entry; replace a corrupt or stale cache file.
    if cache_path.exists() && read_persistent_cache(key).is_some() {
        let _ = fs::remove_file(&temp_path);
        return Ok(false);
    }
    if cache_path.exists() {
        fs::remove_file(&cache_path)?;
    }
    if let Err(error) = fs::rename(&temp_path, &cache_path) {
        let _ = fs::remove_file(&temp_path);
        if !cache_path.exists() {
            return Err(error);
        }
        return Ok(false);
    }

    let write_count = DISK_CACHE_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    if write_count % 32 == 0 {
        prune_persistent_cache(parent);
    }
    Ok(true)
}

fn prune_persistent_cache(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("tsqwf"))
        })
        .filter_map(|path| {
            let modified = fs::metadata(&path).ok()?.modified().ok()?;
            Some((path, modified))
        })
        .collect();

    if files.len() <= MAX_PERSISTENT_CACHE_ENTRIES {
        return;
    }
    files.sort_by_key(|(_, modified)| *modified);
    let remove_count = files.len() - MAX_PERSISTENT_CACHE_ENTRIES;
    for (path, _) in files.into_iter().take(remove_count) {
        if let Err(error) = fs::remove_file(&path) {
            warn!("Could not prune waveform cache entry {:?}: {error}", path);
        }
    }
}

fn read_u8(reader: &mut impl Read) -> std::io::Result<u8> {
    let mut bytes = [0_u8; 1];
    reader.read_exact(&mut bytes)?;
    Ok(bytes[0])
}

fn read_u32(reader: &mut impl Read) -> std::io::Result<u32> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> std::io::Result<u64> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_u128(reader: &mut impl Read) -> std::io::Result<u128> {
    let mut bytes = [0_u8; 16];
    reader.read_exact(&mut bytes)?;
    Ok(u128::from_le_bytes(bytes))
}

fn read_f32(reader: &mut impl Read) -> std::io::Result<f32> {
    Ok(f32::from_bits(read_u32(reader)?))
}

fn read_f64(reader: &mut impl Read) -> std::io::Result<f64> {
    Ok(f64::from_bits(read_u64(reader)?))
}

fn write_u8(writer: &mut impl Write, value: u8) -> std::io::Result<()> {
    writer.write_all(&[value])
}

fn write_u32(writer: &mut impl Write, value: u32) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_u64(writer: &mut impl Write, value: u64) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_u128(writer: &mut impl Write, value: u128) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_f32(writer: &mut impl Write, value: f32) -> std::io::Result<()> {
    write_u32(writer, value.to_bits())
}

fn write_f64(writer: &mut impl Write, value: f64) -> std::io::Result<()> {
    write_u64(writer, value.to_bits())
}

fn decode_audio_file(path: &Path, buckets: usize) -> AudioFileAnalysis {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) => {
            error!("Audio analysis: cannot open {:?}: {error}", path);
            return AudioFileAnalysis::default();
        }
    };

    let decoder = match rodio::Decoder::new(std::io::BufReader::new(file)) {
        Ok(decoder) => decoder,
        Err(error) => {
            error!("Audio analysis: decode error {:?}: {error}", path);
            return AudioFileAnalysis::default();
        }
    };

    let channels = decoder.channels().get().max(1) as usize;
    let sample_rate = decoder.sample_rate().get().max(1) as f64;
    let reported_duration = decoder.total_duration();

    // Rodio 0.23 decoders yield normalized f32 samples. Do not divide by
    // i16::MAX: that would shrink every peak by ~32768 and flatten the display.
    let samples: Vec<f32> = decoder.map(normalize_decoder_sample).collect();
    if samples.is_empty() {
        error!("Audio analysis: decoder returned no samples for {:?}", path);
        return AudioFileAnalysis::default();
    }

    let duration_secs = reported_duration
        .map(|duration| duration.as_secs_f64())
        .filter(|duration| duration.is_finite() && *duration > 0.0)
        .unwrap_or_else(|| samples.len() as f64 / (channels as f64 * sample_rate));

    let peak = samples
        .iter()
        .fold(0.0_f32, |current, sample| current.max(sample.abs()));
    let waveform: Arc<[f32]> = Arc::from(samples_to_waveform(&samples, buckets));

    debug!(
        "Waveform generated: {} buckets for {:?}; samples={}, channels={}, duration={:.3}s, peak={:.6}",
        waveform.len() / 2,
        path,
        samples.len(),
        channels,
        duration_secs,
        peak
    );

    AudioFileAnalysis {
        waveform,
        duration_secs,
        channels,
    }
}

#[inline]
fn normalize_decoder_sample(sample: f32) -> f32 {
    sample.clamp(-1.0, 1.0)
}

/// Convert normalized decoder samples into [min, max] buckets. Peak normalization
/// is display-only: it makes quiet recordings visible without changing playback gain.
fn samples_to_waveform(samples: &[f32], buckets: usize) -> Vec<f32> {
    if samples.is_empty() || buckets == 0 {
        return Vec::new();
    }

    let peak = samples
        .iter()
        .copied()
        .fold(0.0_f32, |current, sample| current.max(sample.abs()));
    if peak <= f32::EPSILON {
        return vec![0.0; buckets * 2];
    }

    let display_gain = 0.95 / peak;
    let mut result = Vec::with_capacity(buckets * 2);

    for bucket in 0..buckets {
        // Divide the complete sample range proportionally. Using ceil(sample
        // count / bucket count) leaves artificial empty buckets at the end of
        // many files and slightly shifts the visible waveform.
        let start = bucket * samples.len() / buckets;
        let end = (((bucket + 1) * samples.len()) / buckets)
            .max(start + 1)
            .min(samples.len());
        if start >= samples.len() {
            result.extend([0.0, 0.0]);
            continue;
        }

        let (minimum, maximum) = samples[start..end].iter().copied().fold(
            (f32::INFINITY, f32::NEG_INFINITY),
            |(minimum, maximum), sample| {
                let sample = (sample * display_gain).clamp(-1.0, 1.0);
                (minimum.min(sample), maximum.max(sample))
            },
        );
        result.push(minimum);
        result.push(maximum);
    }

    result
}

/// Result of one background waveform-analysis job. Several Cue IDs can share
/// the same result when they reference the same audio file.
pub struct WaveformAnalysisResult {
    pub generation: u64,
    pub cue_ids: Vec<Uuid>,
    pub path: PathBuf,
    pub analysis: Arc<AudioFileAnalysis>,
}

struct WaveformAnalysisJob {
    generation: u64,
    cue_ids: Vec<Uuid>,
    path: PathBuf,
    buckets: usize,
}

/// A small worker pool for non-blocking waveform analysis during project load.
/// Stale jobs are skipped when the user opens another project.
pub struct WaveformAnalysisManager {
    job_sender: Sender<WaveformAnalysisJob>,
    result_receiver: Receiver<WaveformAnalysisResult>,
    generation: Arc<AtomicU64>,
}

impl WaveformAnalysisManager {
    pub fn new(worker_count: usize) -> Self {
        let (job_sender, job_receiver) = unbounded::<WaveformAnalysisJob>();
        let (result_sender, result_receiver) = unbounded::<WaveformAnalysisResult>();
        let generation = Arc::new(AtomicU64::new(0));
        let worker_count = worker_count.max(1);

        info!("Starting {worker_count} background waveform analysis worker(s)");
        for worker_index in 0..worker_count {
            let jobs = job_receiver.clone();
            let results = result_sender.clone();
            let active_generation = generation.clone();
            let spawn_result = thread::Builder::new()
                .name(format!("tasq-waveform-{worker_index}"))
                .spawn(move || {
                    while let Ok(job) = jobs.recv() {
                        if active_generation.load(Ordering::Acquire) != job.generation {
                            trace!(
                                "Skipping stale waveform job for {:?}, generation={}",
                                job.path,
                                job.generation
                            );
                            continue;
                        }

                        let analysis = analyze_audio_file(&job.path, job.buckets);
                        if active_generation.load(Ordering::Acquire) != job.generation {
                            continue;
                        }

                        if results
                            .send(WaveformAnalysisResult {
                                generation: job.generation,
                                cue_ids: job.cue_ids,
                                path: job.path,
                                analysis,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            if let Err(error) = spawn_result {
                error!("Could not start waveform analysis worker {worker_index}: {error}");
            }
        }

        Self {
            job_sender,
            result_receiver,
            generation,
        }
    }

    /// Start a new project generation and invalidate queued work from the old one.
    pub fn begin_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn submit(
        &self,
        generation: u64,
        path: PathBuf,
        cue_ids: Vec<Uuid>,
        buckets: usize,
    ) -> bool {
        self.job_sender
            .send(WaveformAnalysisJob {
                generation,
                cue_ids,
                path,
                buckets,
            })
            .is_ok()
    }

    pub fn try_recv(&self) -> Option<WaveformAnalysisResult> {
        self.result_receiver.try_recv().ok()
    }
}

/// Whether an asynchronous audio import creates a new Cue or replaces media
/// on an existing Cue. The Cue ID is reserved by the GUI before the job starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioImportKind {
    Add,
    Replace,
}

pub struct AudioImportResult {
    pub generation: u64,
    pub cue_id: Uuid,
    pub kind: AudioImportKind,
    pub source_path: PathBuf,
    pub destination_path: Option<PathBuf>,
    pub analysis: Option<Arc<AudioFileAnalysis>>,
    pub error: Option<String>,
}

struct AudioImportJob {
    generation: u64,
    cue_id: Uuid,
    kind: AudioImportKind,
    source_path: PathBuf,
    project_path: Option<PathBuf>,
    buckets: usize,
}

/// Performs project-asset copying and audio metadata analysis away from the GUI
/// thread. A single worker preserves submission order, which is important when
/// several files are inserted after the same selected Cue.
pub struct AudioImportManager {
    job_sender: Sender<AudioImportJob>,
    result_receiver: Receiver<AudioImportResult>,
    generation: Arc<AtomicU64>,
    worker_started: bool,
}

impl AudioImportManager {
    pub fn new() -> Self {
        let (job_sender, job_receiver) = unbounded::<AudioImportJob>();
        let (result_sender, result_receiver) = unbounded::<AudioImportResult>();
        let generation = Arc::new(AtomicU64::new(0));
        let active_generation = generation.clone();

        let spawn_result = thread::Builder::new()
            .name("tasq-audio-import".to_string())
            .spawn(move || {
                while let Ok(job) = job_receiver.recv() {
                    if active_generation.load(Ordering::Acquire) != job.generation {
                        trace!("Skipping stale audio import for {:?}", job.source_path);
                        continue;
                    }

                    let imported =
                        (|| -> std::result::Result<(PathBuf, Arc<AudioFileAnalysis>), String> {
                            if !job.source_path.is_file() {
                                return Err(format!(
                                    "Audio file does not exist: {}",
                                    job.source_path.display()
                                ));
                            }

                            let destination = if let Some(project_path) = job.project_path.as_ref()
                            {
                                let mut assets = AssetManager::new();
                                assets.set_project_path(project_path.clone());
                                assets.import_file(&job.source_path).map_err(|error| {
                                    format!("Could not copy audio asset: {error}")
                                })?
                            } else {
                                job.source_path.clone()
                            };

                            if active_generation.load(Ordering::Acquire) != job.generation {
                                return Err(
                                    "Audio import was cancelled because the project changed"
                                        .to_string(),
                                );
                            }

                            let analysis = analyze_audio_file(&destination, job.buckets);
                            Ok((destination, analysis))
                        })();

                    if active_generation.load(Ordering::Acquire) != job.generation {
                        continue;
                    }

                    let (destination_path, analysis, error) = match imported {
                        Ok((path, analysis)) => (Some(path), Some(analysis), None),
                        Err(error) => (None, None, Some(error)),
                    };

                    if result_sender
                        .send(AudioImportResult {
                            generation: job.generation,
                            cue_id: job.cue_id,
                            kind: job.kind,
                            source_path: job.source_path,
                            destination_path,
                            analysis,
                            error,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });

        let worker_started = match spawn_result {
            Ok(worker) => {
                drop(worker);
                true
            }
            Err(error) => {
                error!("Could not start asynchronous audio import worker: {error}");
                false
            }
        };

        Self {
            job_sender,
            result_receiver,
            generation,
            worker_started,
        }
    }

    pub fn begin_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn submit(
        &self,
        generation: u64,
        cue_id: Uuid,
        kind: AudioImportKind,
        source_path: PathBuf,
        project_path: Option<PathBuf>,
        buckets: usize,
    ) -> bool {
        if !self.worker_started {
            return false;
        }
        self.job_sender
            .send(AudioImportJob {
                generation,
                cue_id,
                kind,
                source_path,
                project_path,
                buckets,
            })
            .is_ok()
    }

    pub fn try_recv(&self) -> Option<AudioImportResult> {
        self.result_receiver.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_decoder_sample, samples_to_waveform, AudioAnalysisCache, AudioAnalysisCacheKey,
        AudioFileAnalysis,
    };
    use std::sync::Arc;
    use uuid::Uuid;

    fn key(path: &str, file_size: u64, modified_nanos: u128) -> AudioAnalysisCacheKey {
        AudioAnalysisCacheKey {
            canonical_path: path.to_string(),
            file_size,
            modified_nanos: Some(modified_nanos),
            buckets: 4096,
        }
    }

    fn analysis(value: f32) -> Arc<AudioFileAnalysis> {
        Arc::new(AudioFileAnalysis {
            waveform: Arc::from(vec![value, value]),
            duration_secs: 1.0,
            channels: 2,
        })
    }

    #[test]
    fn rodio_float_samples_keep_their_normalized_amplitude() {
        assert!((normalize_decoder_sample(0.5) - 0.5).abs() < f32::EPSILON);
        assert_eq!(normalize_decoder_sample(1.5), 1.0);
        assert_eq!(normalize_decoder_sample(-1.5), -1.0);
    }

    #[test]
    fn waveform_contains_positive_and_negative_peaks() {
        let samples = [-0.8, -0.4, 0.2, 0.9, 0.1, -0.6, 0.3, 0.7];
        let waveform = samples_to_waveform(&samples, 2);
        assert_eq!(waveform.len(), 4);
        assert!(waveform[0] < -0.7);
        assert!(waveform[1] > 0.8);
        assert!(waveform[2] < -0.5);
        assert!(waveform[3] > 0.6);
    }

    #[test]
    fn quiet_audio_is_boosted_for_display_only() {
        let samples = [-0.01, -0.005, 0.004, 0.01];
        let waveform = samples_to_waveform(&samples, 1);
        assert!(waveform[0] <= -0.3);
        assert!(waveform[1] >= 0.3);
    }

    #[test]
    fn silent_audio_remains_a_flatline() {
        assert_eq!(samples_to_waveform(&[0.0; 8], 4), vec![0.0; 8]);
    }

    #[test]
    fn cache_reuses_the_same_shared_analysis() {
        let mut cache = AudioAnalysisCache::with_capacity(4);
        let key = key("show/audio/sting.wav", 128, 10);
        let expected = analysis(0.5);

        cache.insert(key.clone(), expected.clone(), true);
        let actual = cache.get(&key).expect("cache entry should be found");

        assert!(Arc::ptr_eq(&expected, &actual));
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().decodes, 1);
    }

    #[test]
    fn cache_invalidates_previous_file_fingerprint() {
        let mut cache = AudioAnalysisCache::with_capacity(4);
        let old_key = key("show/audio/sting.wav", 128, 10);
        let new_key = key("show/audio/sting.wav", 256, 20);

        cache.insert(old_key.clone(), analysis(0.25), true);
        cache.insert(new_key.clone(), analysis(0.75), true);

        assert!(cache.get(&old_key).is_none());
        assert!(cache.get(&new_key).is_some());
        assert_eq!(cache.stats().entries, 1);
    }

    #[test]
    fn persistent_cache_round_trip_preserves_waveform_and_metadata() {
        use super::{persistent_cache_path, read_persistent_cache, write_persistent_cache};

        let key = AudioAnalysisCacheKey {
            canonical_path: format!("test-waveform-cache-{}", Uuid::new_v4()),
            file_size: 4096,
            modified_nanos: Some(123_456_789),
            buckets: 2,
        };
        let expected = AudioFileAnalysis {
            waveform: Arc::from(vec![-0.75_f32, 0.5_f32, -0.25_f32, 0.95_f32]),
            duration_secs: 12.5,
            channels: 2,
        };

        let Some(cache_path) = persistent_cache_path(&key) else {
            return;
        };
        assert!(write_persistent_cache(&key, &expected).unwrap());
        let actual = read_persistent_cache(&key).expect("persistent cache entry should load");

        assert_eq!(actual.waveform.as_ref(), expected.waveform.as_ref());
        assert_eq!(actual.duration_secs, expected.duration_secs);
        assert_eq!(actual.channels, expected.channels);
        let _ = std::fs::remove_file(cache_path);
    }

    #[test]
    fn cache_evicts_least_recently_used_entries_at_capacity() {
        let mut cache = AudioAnalysisCache::with_capacity(2);
        let first = key("show/audio/first.wav", 10, 1);
        let second = key("show/audio/second.wav", 10, 1);
        let third = key("show/audio/third.wav", 10, 1);

        cache.insert(first.clone(), analysis(0.1), true);
        cache.insert(second.clone(), analysis(0.2), true);
        assert!(cache.get(&first).is_some());
        cache.insert(third.clone(), analysis(0.3), true);

        assert!(cache.get(&first).is_some());
        assert!(cache.get(&second).is_none());
        assert!(cache.get(&third).is_some());
        assert_eq!(cache.stats().evictions, 1);
    }
}
