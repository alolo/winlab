//! Test-only sparse endpoint mixer prototype.
//!
//! This module is included from `engine.rs` under `#[cfg(test)]`. It deliberately
//! does not replace the production Rodio mixer. The prototype renders one final
//! endpoint frame by pulling each Cue's source frame once and accumulating only
//! its configured destination routes.

use rodio::Source;
use std::num::{NonZeroU16, NonZeroU32};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
struct SparseDestination {
    output_channel: usize,
    gain_linear: f32,
}

struct SparseCue {
    source: Box<dyn Source<Item = f32> + Send>,
    sample_rate: u32,
    input_frame: Vec<f32>,
    routes: Vec<Vec<SparseDestination>>,
    finished: bool,
}

impl SparseCue {
    fn new<S>(
        source: S,
        mut routes: Vec<Vec<SparseDestination>>,
        output_channels: usize,
    ) -> Result<Self, String>
    where
        S: Source<Item = f32> + Send + 'static,
    {
        if output_channels == 0 || output_channels > u16::MAX as usize {
            return Err(format!("invalid output channel count: {output_channels}"));
        }

        let input_channels = source.channels().get() as usize;
        if routes.len() != input_channels {
            return Err(format!(
                "route channel count {} does not match source channel count {input_channels}",
                routes.len()
            ));
        }

        let mut seen_destinations = vec![false; output_channels];
        for (source_channel, destinations) in routes.iter_mut().enumerate() {
            seen_destinations.fill(false);
            destinations.retain(|destination| {
                if destination.output_channel >= output_channels
                    || !destination.gain_linear.is_finite()
                {
                    return true;
                }
                let duplicate = seen_destinations[destination.output_channel];
                seen_destinations[destination.output_channel] = true;
                if duplicate {
                    return false;
                }
                true
            });
            if let Some(destination) = destinations
                .iter()
                .find(|destination| destination.output_channel >= output_channels)
            {
                return Err(format!(
                    "source channel {source_channel} routes to output {}, but endpoint has only {output_channels} channels",
                    destination.output_channel
                ));
            }
            if destinations
                .iter()
                .any(|destination| !destination.gain_linear.is_finite())
            {
                return Err(format!(
                    "source channel {source_channel} has a non-finite route gain"
                ));
            }
        }

        Ok(Self {
            sample_rate: source.sample_rate().get(),
            source: Box::new(source),
            input_frame: vec![0.0; input_channels],
            routes,
            finished: false,
        })
    }

    /// Pull exactly one complete source frame and add its sparse routes to the
    /// already-cleared endpoint frame. No allocation, lock, or blocking call is
    /// performed here. A partial final source frame is discarded rather than
    /// allowing channel alignment to drift.
    #[inline]
    fn mix_next_frame_into(&mut self, output_frame: &mut [f32]) {
        if self.finished {
            return;
        }

        for sample in &mut self.input_frame {
            match self.source.next() {
                Some(value) => *sample = value,
                None => {
                    self.finished = true;
                    return;
                }
            }
        }

        for (sample, destinations) in self.input_frame.iter().zip(self.routes.iter()) {
            if *sample == 0.0 {
                continue;
            }
            for destination in destinations {
                // Destination indices are validated at construction.
                output_frame[destination.output_channel] += *sample * destination.gain_linear;
            }
        }
    }
}

/// Fixed-membership prototype. Cue sources and route snapshots are prepared
/// before rendering; dynamic add/remove/live route handoff is intentionally not
/// implemented until the benchmark validates the core rendering model.
struct SparseEndpointMixer {
    cues: Vec<SparseCue>,
    output_channels: usize,
    sample_rate: u32,
    output_frame: Vec<f32>,
    output_index: usize,
}

impl SparseEndpointMixer {
    fn new(output_channels: usize, sample_rate: u32, cues: Vec<SparseCue>) -> Result<Self, String> {
        if output_channels == 0 || output_channels > u16::MAX as usize {
            return Err(format!("invalid output channel count: {output_channels}"));
        }
        if sample_rate == 0 {
            return Err("sample rate must be greater than zero".to_string());
        }
        if let Some(cue) = cues.iter().find(|cue| cue.sample_rate != sample_rate) {
            return Err(format!(
                "Cue sample rate {} does not match endpoint sample rate {sample_rate}; resample each Cue before sparse routing",
                cue.sample_rate
            ));
        }

        Ok(Self {
            cues,
            output_channels,
            sample_rate,
            output_frame: vec![0.0; output_channels],
            output_index: output_channels,
        })
    }

    #[inline]
    fn render_endpoint_frame(&mut self) {
        self.output_frame.fill(0.0);
        for cue in &mut self.cues {
            cue.mix_next_frame_into(&mut self.output_frame);
        }
        self.output_index = 0;
    }
}

impl Iterator for SparseEndpointMixer {
    type Item = f32;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.output_index >= self.output_channels {
            self.render_endpoint_frame();
        }

        let sample = self.output_frame[self.output_index];
        self.output_index += 1;
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

impl Source for SparseEndpointMixer {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> NonZeroU16 {
        NonZeroU16::new(self.output_channels as u16)
            .expect("SparseEndpointMixer validates a non-zero output channel count")
    }

    fn sample_rate(&self) -> NonZeroU32 {
        NonZeroU32::new(self.sample_rate)
            .expect("SparseEndpointMixer validates a non-zero sample rate")
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{SparseCue, SparseDestination, SparseEndpointMixer};
    use crate::cue::{ChannelRoute, OutputChannelSetting};
    use parking_lot::RwLock;
    use rodio::buffer::SamplesBuffer;
    use std::num::{NonZeroU16, NonZeroU32};
    use std::sync::Arc;
    use std::time::Instant;

    use super::super::{ChannelRouter, SharedOutputSettings};

    const OUTPUT_CHANNELS: usize = 128;
    const SAMPLE_RATE: u32 = 48_000;

    fn mono_source(value: f32, frames: usize) -> SamplesBuffer {
        SamplesBuffer::new(
            NonZeroU16::new(1).unwrap(),
            NonZeroU32::new(SAMPLE_RATE).unwrap(),
            vec![value; frames],
        )
    }

    fn stereo_source(left: f32, right: f32, frames: usize) -> SamplesBuffer {
        let mut samples = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            samples.push(left);
            samples.push(right);
        }
        SamplesBuffer::new(
            NonZeroU16::new(2).unwrap(),
            NonZeroU32::new(SAMPLE_RATE).unwrap(),
            samples,
        )
    }

    fn destinations(indices: impl IntoIterator<Item = usize>) -> Vec<SparseDestination> {
        indices
            .into_iter()
            .map(|output_channel| SparseDestination {
                output_channel,
                gain_linear: 1.0,
            })
            .collect()
    }

    fn route(destinations: Vec<usize>) -> ChannelRoute {
        ChannelRoute {
            active: !destinations.is_empty(),
            master_enabled: true,
            output_channel: destinations.first().copied().unwrap_or(0),
            output_channels: destinations,
            gain_db: 0.0,
        }
    }

    fn shared_output_settings() -> SharedOutputSettings {
        Arc::new(RwLock::new(vec![
            OutputChannelSetting::default();
            OUTPUT_CHANNELS
        ]))
    }

    fn output_gains() -> Arc<RwLock<Vec<f32>>> {
        Arc::new(RwLock::new(vec![0.0; OUTPUT_CHANNELS]))
    }

    #[derive(Clone)]
    struct TestCueSpec {
        source_channels: usize,
        samples: Vec<f32>,
        routes: Vec<Vec<usize>>,
    }

    fn deterministic_samples(cue_index: usize, source_channels: usize, frames: usize) -> Vec<f32> {
        let mut samples = Vec::with_capacity(source_channels * frames);
        for frame in 0..frames {
            for source_channel in 0..source_channels {
                // Dyadic fractions make the reference/sparse sum reproducible while
                // still providing different, time-varying signals per Cue/channel.
                let mixed = (frame * 37
                    + cue_index * 97
                    + source_channel * 211
                    + (frame * (cue_index + 3)) % 127)
                    % 2047;
                let signed = mixed as i32 - 1023;
                samples.push(signed as f32 / 8192.0);
            }
        }
        samples
    }

    fn deterministic_cues(
        cue_count: usize,
        source_channels: usize,
        routes: Vec<Vec<usize>>,
        frames: usize,
    ) -> Vec<TestCueSpec> {
        (0..cue_count)
            .map(|cue_index| TestCueSpec {
                source_channels,
                samples: deterministic_samples(cue_index, source_channels, frames),
                routes: routes.clone(),
            })
            .collect()
    }

    fn route_map_for_fanout(
        source_channels: usize,
        fanout: usize,
        both_stereo_channels_to_same_outputs: bool,
    ) -> Vec<Vec<usize>> {
        if source_channels == 1 {
            vec![(0..fanout).collect()]
        } else if both_stereo_channels_to_same_outputs {
            vec![(0..fanout).collect(), (0..fanout).collect()]
        } else {
            let left_count = (fanout + 1) / 2;
            vec![(0..left_count).collect(), (left_count..fanout).collect()]
        }
    }

    fn samples_buffer(spec: &TestCueSpec) -> SamplesBuffer {
        SamplesBuffer::new(
            NonZeroU16::new(spec.source_channels as u16).unwrap(),
            NonZeroU32::new(SAMPLE_RATE).unwrap(),
            spec.samples.clone(),
        )
    }

    fn render_rodio(specs: &[TestCueSpec], frames: usize) -> (std::time::Duration, Vec<f32>) {
        let channels = NonZeroU16::new(OUTPUT_CHANNELS as u16).unwrap();
        let sample_rate = NonZeroU32::new(SAMPLE_RATE).unwrap();
        let (mixer, mut output) = rodio::mixer::mixer(channels, sample_rate);
        let settings = shared_output_settings();
        let gains = output_gains();

        for spec in specs {
            let source = samples_buffer(spec);
            let route_list = spec.routes.iter().cloned().map(route).collect();
            let routed = ChannelRouter::new(
                source,
                Arc::new(RwLock::new(route_list)),
                settings.clone(),
                gains.clone(),
                OUTPUT_CHANNELS,
            );
            mixer.add(routed);
        }

        // Allocate the capture buffer before timing. Both render paths write the
        // same number of endpoint samples into pre-sized-capacity Vecs.
        let mut rendered = Vec::with_capacity(frames * OUTPUT_CHANNELS);
        let started = Instant::now();
        for _ in 0..frames * OUTPUT_CHANNELS {
            rendered.push(
                output
                    .next()
                    .expect("Rodio mixer should render silence or audio"),
            );
        }
        (started.elapsed(), rendered)
    }

    fn render_sparse(specs: &[TestCueSpec], frames: usize) -> (std::time::Duration, Vec<f32>) {
        let mut cues = Vec::with_capacity(specs.len());
        for spec in specs {
            let source = samples_buffer(spec);
            let sparse_routes = spec
                .routes
                .iter()
                .map(|destinations_for_channel| {
                    destinations(destinations_for_channel.iter().copied())
                })
                .collect();
            cues.push(
                SparseCue::new(source, sparse_routes, OUTPUT_CHANNELS)
                    .expect("valid sparse route map"),
            );
        }
        let mut mixer = SparseEndpointMixer::new(OUTPUT_CHANNELS, SAMPLE_RATE, cues)
            .expect("valid sparse endpoint mixer");

        let mut rendered = Vec::with_capacity(frames * OUTPUT_CHANNELS);
        let started = Instant::now();
        for _ in 0..frames * OUTPUT_CHANNELS {
            rendered.push(mixer.next().expect("sparse mixer renders audio"));
        }
        (started.elapsed(), rendered)
    }

    fn assert_sample_by_sample_equal(label: &str, expected: &[f32], actual: &[f32]) {
        assert_eq!(
            expected.len(),
            actual.len(),
            "{label}: endpoint sample count differs"
        );

        let mut max_abs_error = 0.0_f32;
        let mut max_error_index = 0_usize;
        for (index, (&expected_sample, &actual_sample)) in
            expected.iter().zip(actual.iter()).enumerate()
        {
            let abs_error = (expected_sample - actual_sample).abs();
            if abs_error > max_abs_error {
                max_abs_error = abs_error;
                max_error_index = index;
            }
            assert!(
                abs_error <= 1.0e-6,
                "{label}: sample mismatch at interleaved index {index} \
                 (frame {}, output channel {}): expected {expected_sample:?}, \
                 actual {actual_sample:?}, abs_error={abs_error}",
                index / OUTPUT_CHANNELS,
                index % OUTPUT_CHANNELS,
            );
        }

        assert!(
            max_abs_error <= 1.0e-6,
            "{label}: max sample error {max_abs_error} at interleaved index {max_error_index}"
        );
    }

    #[test]
    fn mono_cue_can_fan_out_to_ten_outputs() {
        let cue = SparseCue::new(
            mono_source(0.25, 2),
            vec![destinations(0..10)],
            OUTPUT_CHANNELS,
        )
        .unwrap();
        let mut mixer = SparseEndpointMixer::new(OUTPUT_CHANNELS, SAMPLE_RATE, vec![cue]).unwrap();

        let rendered = (0..OUTPUT_CHANNELS)
            .map(|_| mixer.next().unwrap())
            .collect::<Vec<_>>();
        for sample in &rendered[..10] {
            assert_eq!(*sample, 0.25);
        }
        assert!(rendered[10..].iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn mono_cue_can_fan_out_to_sixteen_outputs() {
        let cue = SparseCue::new(
            mono_source(0.25, 2),
            vec![destinations(0..16)],
            OUTPUT_CHANNELS,
        )
        .unwrap();
        let mut mixer = SparseEndpointMixer::new(OUTPUT_CHANNELS, SAMPLE_RATE, vec![cue]).unwrap();

        let rendered = (0..OUTPUT_CHANNELS)
            .map(|_| mixer.next().unwrap())
            .collect::<Vec<_>>();
        assert!(rendered[..16].iter().all(|sample| *sample == 0.25));
        assert!(rendered[16..].iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn stereo_cue_can_route_to_ten_distinct_outputs() {
        let cue = SparseCue::new(
            stereo_source(0.125, 0.25, 1),
            vec![destinations(0..5), destinations(5..10)],
            OUTPUT_CHANNELS,
        )
        .unwrap();
        let mut mixer = SparseEndpointMixer::new(OUTPUT_CHANNELS, SAMPLE_RATE, vec![cue]).unwrap();
        let rendered = (0..OUTPUT_CHANNELS)
            .map(|_| mixer.next().unwrap())
            .collect::<Vec<_>>();

        assert!(rendered[..5].iter().all(|sample| *sample == 0.125));
        assert!(rendered[5..10].iter().all(|sample| *sample == 0.25));
        assert!(rendered[10..].iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn stereo_cue_can_route_to_sixteen_distinct_outputs() {
        let left = destinations(0..8);
        let right = destinations(8..16);
        let cue = SparseCue::new(
            stereo_source(0.125, 0.25, 1),
            vec![left, right],
            OUTPUT_CHANNELS,
        )
        .unwrap();
        let mut mixer = SparseEndpointMixer::new(OUTPUT_CHANNELS, SAMPLE_RATE, vec![cue]).unwrap();
        let rendered = (0..OUTPUT_CHANNELS)
            .map(|_| mixer.next().unwrap())
            .collect::<Vec<_>>();

        assert!(rendered[..8].iter().all(|sample| *sample == 0.125));
        assert!(rendered[8..16].iter().all(|sample| *sample == 0.25));
        assert!(rendered[16..].iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn each_stereo_source_channel_can_fan_out_to_sixteen_outputs() {
        let cue = SparseCue::new(
            stereo_source(0.125, 0.25, 1),
            vec![destinations(0..16), destinations(0..16)],
            OUTPUT_CHANNELS,
        )
        .unwrap();
        let mut mixer = SparseEndpointMixer::new(OUTPUT_CHANNELS, SAMPLE_RATE, vec![cue]).unwrap();
        let rendered = (0..OUTPUT_CHANNELS)
            .map(|_| mixer.next().unwrap())
            .collect::<Vec<_>>();

        assert!(rendered[..16].iter().all(|sample| *sample == 0.375));
        assert!(rendered[16..].iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn route_gain_is_applied_independently_per_destination() {
        let cue = SparseCue::new(
            mono_source(0.5, 1),
            vec![vec![
                SparseDestination {
                    output_channel: 0,
                    gain_linear: 0.5,
                },
                SparseDestination {
                    output_channel: 1,
                    gain_linear: 2.0,
                },
            ]],
            OUTPUT_CHANNELS,
        )
        .unwrap();
        let mut mixer = SparseEndpointMixer::new(OUTPUT_CHANNELS, SAMPLE_RATE, vec![cue]).unwrap();
        let first = mixer.next().unwrap();
        let second = mixer.next().unwrap();

        assert_eq!(first, 0.25);
        assert_eq!(second, 1.0);
    }

    #[test]
    fn multiple_cues_sum_correctly_on_shared_outputs() {
        let routes = vec![destinations([0, 1, 126, 127])];
        let first = SparseCue::new(mono_source(0.25, 1), routes.clone(), OUTPUT_CHANNELS).unwrap();
        let second = SparseCue::new(mono_source(0.5, 1), routes, OUTPUT_CHANNELS).unwrap();
        let mut mixer =
            SparseEndpointMixer::new(OUTPUT_CHANNELS, SAMPLE_RATE, vec![first, second]).unwrap();
        let rendered = (0..OUTPUT_CHANNELS)
            .map(|_| mixer.next().unwrap())
            .collect::<Vec<_>>();

        for index in [0, 1, 126, 127] {
            assert_eq!(rendered[index], 0.75);
        }
        for (index, sample) in rendered.iter().enumerate() {
            if ![0, 1, 126, 127].contains(&index) {
                assert_eq!(*sample, 0.0);
            }
        }
    }

    #[test]
    fn sparse_mixer_rejects_out_of_range_destinations() {
        let result = SparseCue::new(
            mono_source(0.25, 1),
            vec![destinations([OUTPUT_CHANNELS])],
            OUTPUT_CHANNELS,
        );
        assert!(result.is_err());
    }

    #[test]
    fn sample_by_sample_parity_covers_all_route_shapes() {
        // 257 frames deliberately exercises non-round frame counts. Every
        // comparison checks all 128 output channels for every rendered frame.
        let frames = 257;
        let scenarios = vec![
            (
                "1 mono Cue / 10 outputs",
                1,
                1,
                route_map_for_fanout(1, 10, false),
            ),
            (
                "1 stereo Cue / 10 distinct outputs",
                1,
                2,
                route_map_for_fanout(2, 10, false),
            ),
            (
                "1 mono Cue / 16 outputs",
                1,
                1,
                route_map_for_fanout(1, 16, false),
            ),
            (
                "1 stereo Cue / 16 distinct outputs",
                1,
                2,
                route_map_for_fanout(2, 16, false),
            ),
            (
                "1 stereo Cue / endpoint outputs 112 through 127",
                1,
                2,
                vec![(112..120).collect(), (120..128).collect()],
            ),
            (
                "1 stereo Cue / 32 route writes to 16 shared outputs",
                1,
                2,
                route_map_for_fanout(2, 16, true),
            ),
            (
                "4 stereo Cue / 2 outputs",
                4,
                2,
                route_map_for_fanout(2, 2, false),
            ),
            (
                "4 stereo Cue / 16 distinct outputs",
                4,
                2,
                route_map_for_fanout(2, 16, false),
            ),
            (
                "15 stereo Cue / 2 outputs",
                15,
                2,
                route_map_for_fanout(2, 2, false),
            ),
            (
                "15 stereo Cue / 16 distinct outputs",
                15,
                2,
                route_map_for_fanout(2, 16, false),
            ),
        ];

        for (label, cue_count, source_channels, routes) in scenarios {
            let specs = deterministic_cues(cue_count, source_channels, routes, frames);
            let (_, expected) = render_rodio(&specs, frames);
            let (_, actual) = render_sparse(&specs, frames);
            assert_sample_by_sample_equal(label, &expected, &actual);
        }
    }

    #[test]
    fn fifteen_distinct_stereo_cues_sum_sample_by_sample_on_same_sixteen_outputs() {
        let frames = 513;
        let shared_routes = route_map_for_fanout(2, 16, true);
        let specs = deterministic_cues(15, 2, shared_routes, frames);

        assert_ne!(
            specs[0].samples, specs[1].samples,
            "test Cue signals must be different"
        );
        assert_ne!(
            specs[1].samples, specs[14].samples,
            "test Cue signals must be different"
        );

        let (_, expected) = render_rodio(&specs, frames);
        let (_, actual) = render_sparse(&specs, frames);
        assert_sample_by_sample_equal(
            "15 stereo Cue, each source channel routed to the same 16 outputs",
            &expected,
            &actual,
        );
    }

    #[test]
    fn sparse_mixer_output_matches_channel_router_for_sixteen_output_stereo_cue() {
        let frames = 8;
        let route_list = vec![route((0..8).collect()), route((8..16).collect())];
        let settings = shared_output_settings();
        let gains = output_gains();
        let (rodio_mixer, mut rodio_output) = rodio::mixer::mixer(
            NonZeroU16::new(OUTPUT_CHANNELS as u16).unwrap(),
            NonZeroU32::new(SAMPLE_RATE).unwrap(),
        );
        let routed = ChannelRouter::new(
            stereo_source(0.125, 0.25, frames),
            Arc::new(RwLock::new(route_list)),
            settings,
            gains,
            OUTPUT_CHANNELS,
        );
        rodio_mixer.add(routed);

        let sparse_cue = SparseCue::new(
            stereo_source(0.125, 0.25, frames),
            vec![destinations(0..8), destinations(8..16)],
            OUTPUT_CHANNELS,
        )
        .unwrap();
        let mut sparse_mixer =
            SparseEndpointMixer::new(OUTPUT_CHANNELS, SAMPLE_RATE, vec![sparse_cue]).unwrap();

        for _ in 0..frames * OUTPUT_CHANNELS {
            let expected = rodio_output.next().unwrap();
            let actual = sparse_mixer.next().unwrap();
            assert!((actual - expected).abs() <= f32::EPSILON);
        }
    }

    #[test]
    #[ignore = "manual release-mode performance benchmark; see Stage 3.1 README"]
    fn benchmark_sparse_mixer_against_rodio_for_one_four_and_fifteen_cues() {
        // 250 ms of 48 kHz audio per case. Source buffers, route maps and
        // capture buffers are prepared before each measured render.
        let frames = SAMPLE_RATE as usize / 4;
        let cases: Vec<(&str, usize, usize, Vec<Vec<usize>>)> = vec![
            ("1 mono / 1 output", 1, 1, route_map_for_fanout(1, 1, false)),
            (
                "1 mono / 2 outputs",
                1,
                1,
                route_map_for_fanout(1, 2, false),
            ),
            (
                "1 mono / 10 outputs",
                1,
                1,
                route_map_for_fanout(1, 10, false),
            ),
            (
                "1 mono / 16 outputs",
                1,
                1,
                route_map_for_fanout(1, 16, false),
            ),
            (
                "1 stereo / 10 distinct outputs",
                1,
                2,
                route_map_for_fanout(2, 10, false),
            ),
            (
                "1 stereo / 16 distinct outputs",
                1,
                2,
                route_map_for_fanout(2, 16, false),
            ),
            (
                "1 stereo / 32 writes to 16 outputs",
                1,
                2,
                route_map_for_fanout(2, 16, true),
            ),
            (
                "4 stereo / 2 outputs",
                4,
                2,
                route_map_for_fanout(2, 2, false),
            ),
            (
                "4 stereo / 16 distinct outputs",
                4,
                2,
                route_map_for_fanout(2, 16, false),
            ),
            (
                "4 stereo / 32 writes to 16 outputs",
                4,
                2,
                route_map_for_fanout(2, 16, true),
            ),
            (
                "15 stereo / 2 outputs",
                15,
                2,
                route_map_for_fanout(2, 2, false),
            ),
            (
                "15 stereo / 16 distinct outputs",
                15,
                2,
                route_map_for_fanout(2, 16, false),
            ),
            (
                "15 stereo / 32 writes to 16 outputs",
                15,
                2,
                route_map_for_fanout(2, 16, true),
            ),
        ];

        println!("Sparse mixer benchmark: {frames} frames, 128 outputs, 48 kHz");
        println!(
            "scenario,cues,source_channels,route_writes_per_cue,rodio_ms,sparse_ms,speedup,checksum"
        );

        for (label, cue_count, source_channels, routes) in cases {
            let specs = deterministic_cues(cue_count, source_channels, routes, frames);
            let route_writes_per_cue = specs[0].routes.iter().map(Vec::len).sum::<usize>();

            let (rodio_elapsed, rodio_samples) = render_rodio(&specs, frames);
            let (sparse_elapsed, sparse_samples) = render_sparse(&specs, frames);

            // Verify every interleaved sample outside the timed sections. A
            // matching aggregate checksum alone is not sufficient for parity.
            assert_sample_by_sample_equal(label, &rodio_samples, &sparse_samples);
            let checksum = sparse_samples
                .iter()
                .map(|sample| *sample as f64)
                .sum::<f64>();
            let speedup = rodio_elapsed.as_secs_f64() / sparse_elapsed.as_secs_f64();

            println!(
                "{label},{cue_count},{source_channels},{route_writes_per_cue},{:.3},{:.3},{:.2}x,{checksum:.6}",
                rodio_elapsed.as_secs_f64() * 1_000.0,
                sparse_elapsed.as_secs_f64() * 1_000.0,
                speedup,
            );
        }
    }
}
