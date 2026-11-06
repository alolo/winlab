// src/audio/waveform.rs — генерация пиков вейвформы и точное определение длительности

use rodio::Source;
use std::path::Path;
use tracing::{debug, error};

/// Сжимает аудиофайл в массив пар [min, max] для отрисовки вейвформы
pub fn generate_waveform(path: &Path, buckets: usize) -> Vec<f32> {
    if path.as_os_str().is_empty() {
        return vec![];
    }
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            error!("Waveform: cannot open {:?}: {e}", path);
            return vec![0.0; buckets * 2];
        }
    };

    let decoder = match rodio::Decoder::new(std::io::BufReader::new(file)) {
        Ok(d) => d,
        Err(e) => {
            error!("Waveform: decode error {:?}: {e}", path);
            return vec![0.0; buckets * 2];
        }
    };

    let samples: Vec<f32> = decoder.map(|s| s as f32 / i16::MAX as f32).collect();

    if samples.is_empty() {
        return vec![0.0; buckets * 2];
    }

    let bucket_size = (samples.len() / buckets).max(1);
    let mut result = Vec::with_capacity(buckets * 2);

    for i in 0..buckets {
        let start = i * bucket_size;
        let end = ((i + 1) * bucket_size).min(samples.len());
        if start >= samples.len() {
            result.push(0.0);
            result.push(0.0);
            continue;
        }
        let slice = &samples[start..end];
        let min = slice.iter().copied().fold(f32::INFINITY, f32::min);
        let max = slice.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        result.push(min.clamp(-1.0, 0.0));
        result.push(max.clamp(0.0, 1.0));
    }

    debug!("Waveform generated: {} buckets for {:?}", buckets, path);
    result
}

/// Точное определение длительности аудиофайла с учетом числа каналов и частоты дискретизации
pub fn get_duration(path: &Path) -> f64 {
    if path.as_os_str().is_empty() {
        return 0.0;
    }
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return 0.0,
    };
    match rodio::Decoder::new(std::io::BufReader::new(file)) {
        Ok(d) => {
            if let Some(dur) = d.total_duration() {
                return dur.as_secs_f64();
            }
            let channels = d.channels().get().max(1) as f64;
            let rate = d.sample_rate().get().max(1) as f64;
            let total_samples = d.count() as f64;
            total_samples / (channels * rate)
        }
        Err(_) => 0.0,
    }
}

/// Returns the number of interleaved audio channels reported by the decoder.
pub fn get_channel_count(path: &Path) -> usize {
    if path.as_os_str().is_empty() {
        return 0;
    }
    let Ok(file) = std::fs::File::open(path) else {
        return 0;
    };
    rodio::Decoder::new(std::io::BufReader::new(file))
        .map(|decoder| decoder.channels().get().max(1) as usize)
        .unwrap_or(0)
}
