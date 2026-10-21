// Waveform analysis — generates peak data for visual display

use std::path::Path;
use tracing::{debug, error};

/// Downsample audio file into normalized peak pairs [min, max] per pixel bucket
pub fn generate_waveform(path: &Path, buckets: usize) -> Vec<f32> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            error!("Waveform: cannot open {:?}: {e}", path);
            return vec![0.0; buckets * 2];
        }
    };

    let mss = rodio::Decoder::new(std::io::BufReader::new(file));
    let decoder = match mss {
        Ok(d) => d,
        Err(e) => {
            error!("Waveform: decode error {:?}: {e}", path);
            return vec![0.0; buckets * 2];
        }
    };

    // Collect all samples (mixed to mono for display)
    let samples: Vec<f32> = decoder
        .map(|s| s as f32 / i16::MAX as f32)
        .collect();

    if samples.is_empty() {
        return vec![0.0; buckets * 2];
    }

    // Compute min/max per bucket
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
        let min = slice.iter().cloned().fold(f32::INFINITY, f32::min);
        let max = slice.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        result.push(min.clamp(-1.0, 0.0));
        result.push(max.clamp(0.0, 1.0));
    }

    debug!("Waveform generated: {} buckets for {:?}", buckets, path);
    result
}

/// Get audio duration in seconds using rodio decoder
pub fn get_duration(path: &Path) -> f64 {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return 0.0,
    };
    match rodio::Decoder::new(std::io::BufReader::new(file)) {
        Ok(d) => {
            d.count() as f64 / 44100.0 // approximate; proper impl uses symphonia
        }
        Err(_) => 0.0,
    }
}
