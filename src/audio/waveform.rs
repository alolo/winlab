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
            return vec![];
        }
    };

    let decoder = match rodio::Decoder::new(std::io::BufReader::new(file)) {
        Ok(d) => d,
        Err(e) => {
            error!("Waveform: decode error {:?}: {e}", path);
            return vec![];
        }
    };

    // Rodio 0.23 decoders yield normalized f32 samples. Do not divide by i16::MAX:
    // that would shrink every peak by ~32768 and make the waveform look flat.
    let samples: Vec<f32> = decoder.map(normalize_decoder_sample).collect();
    let result = samples_to_waveform(&samples, buckets);
    let peak = samples
        .iter()
        .fold(0.0_f32, |current, sample| current.max(sample.abs()));
    debug!(
        "Waveform generated: {} buckets for {:?}; samples={}, peak={:.6}",
        result.len() / 2,
        path,
        samples.len(),
        peak
    );
    result
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

#[cfg(test)]
mod tests {
    use super::{normalize_decoder_sample, samples_to_waveform};

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
}
