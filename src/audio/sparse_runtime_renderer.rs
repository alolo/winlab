//! Stage 4.2b3 test-only sparse endpoint PCM renderer.
//!
//! Each Cue keeps a frame-sized scratch buffer for its own source-channel
//! count. The renderer resamples to the endpoint rate without changing that
//! channel count, then routes/sums directly into one preallocated endpoint
//! frame. Cue payloads remain in the Stage 4.2b2 ownership table; render only
//! borrows Active payloads and never takes ownership or performs retirement.
//!
//! This module is included under `#[cfg(test)]` and is not connected to the
//! production DeviceOutput, CPAL/ASIO, WASAPI, DirectSound or Rodio playback.

use super::db_to_linear;
use super::sparse_runtime_contract::{CueKey, SlotError, SlotToken};
use super::sparse_runtime_control::SparseCueRenderControl;
use super::sparse_runtime_ownership::{
    AckReport, AddSourceError, CueSourceOwnership, CueSourcePayload, RetireAllReport, RetireStatus,
    RetryReport,
};
use crate::cue::{ChannelRoute, OutputChannelSetting};
use rodio::Source;
use std::num::{NonZeroU16, NonZeroU32};

pub(crate) type SparseSource = Box<dyn Source<Item = f32> + Send>;
pub(crate) type SparseRendererPayload = CueSourcePayload<SparseSource, SparseCueRenderResources>;
pub(crate) type SparseRendererOwnership =
    CueSourceOwnership<SparseSource, SparseCueRenderResources>;

#[derive(Debug, Clone, Copy)]
struct SparseDestination {
    output_channel: usize,
    cue_output_gain_linear: f32,
}

struct SparseSourceChannelRoute {
    active: bool,
    master_enabled: bool,
    gain_linear: f32,
    destinations: Vec<SparseDestination>,
}

#[derive(Clone, Copy)]
struct SparseEndpointOutputSetting {
    active: bool,
    gain_linear: f32,
}

pub(crate) struct SparseCueRenderResources {
    /// Exactly one interleaved source frame; never endpoint-sized.
    input_frame: Vec<f32>,
    routes: Vec<SparseSourceChannelRoute>,
    cue_trim: f32,
    control: Option<SparseCueRenderControl>,
    finished: bool,
}

impl SparseCueRenderResources {
    fn new(
        source_channels: usize,
        output_channels: usize,
        routes: &[ChannelRoute],
        cue_output_gains_db: &[f32],
        cue_trim: f32,
        control: Option<SparseCueRenderControl>,
    ) -> Self {
        let mut prepared_routes = Vec::with_capacity(source_channels);

        for source_index in 0..source_channels {
            let route = routes.get(source_index);
            let active = route.map(|route| route.active).unwrap_or(true);
            let master_enabled = route.map(|route| route.master_enabled).unwrap_or(true);
            let fallback_output = route
                .map(|route| route.output_channel)
                .unwrap_or(source_index)
                .min(output_channels - 1);
            let gain_linear = db_to_linear(route.map(|route| route.gain_db).unwrap_or(0.0));

            let mut destinations = Vec::new();
            if let Some(route) = route {
                for &output_channel in &route.output_channels {
                    if output_channel < output_channels
                        && !destinations.iter().any(|destination: &SparseDestination| {
                            destination.output_channel == output_channel
                        })
                    {
                        destinations.push(SparseDestination {
                            output_channel,
                            cue_output_gain_linear: db_to_linear(
                                cue_output_gains_db
                                    .get(output_channel)
                                    .copied()
                                    .unwrap_or(0.0),
                            ),
                        });
                    }
                }
            }

            // Match ChannelRouter's fallback for a missing route or an active
            // route whose explicit destinations were empty/invalid.
            if destinations.is_empty() && active {
                destinations.push(SparseDestination {
                    output_channel: fallback_output,
                    cue_output_gain_linear: db_to_linear(
                        cue_output_gains_db
                            .get(fallback_output)
                            .copied()
                            .unwrap_or(0.0),
                    ),
                });
            }

            destinations.sort_unstable_by_key(|destination| destination.output_channel);
            prepared_routes.push(SparseSourceChannelRoute {
                active,
                master_enabled,
                gain_linear,
                destinations,
            });
        }

        Self {
            input_frame: vec![0.0; source_channels],
            routes: prepared_routes,
            cue_trim,
            control,
            finished: false,
        }
    }

    fn is_finished(&self) -> bool {
        self.finished
            || self
                .control
                .as_ref()
                .is_some_and(SparseCueRenderControl::is_finished)
    }

    /// Pulls one complete source-channel frame and sums only configured routes.
    /// All vectors and endpoint scratch are prepared before rendering.
    #[inline]
    fn mix_next_frame_into(
        &mut self,
        source: &mut SparseSource,
        output_frame: &mut [f32],
        output_settings: &[SparseEndpointOutputSetting],
        master_gain: f32,
    ) {
        if self.finished {
            return;
        }

        if let Some(control) = &self.control {
            if !control.begin_endpoint_frame() {
                self.finished = control.is_finished();
                return;
            }
        }

        for sample in &mut self.input_frame {
            match source.next() {
                Some(value) => *sample = value,
                None => {
                    // Discard a partial final frame; never rotate channel phase.
                    // Keep the source owned by its slot until the control side
                    // transfers the complete payload to the Reaper.
                    self.finished = true;
                    if let Some(control) = &self.control {
                        control.mark_finished();
                    }
                    return;
                }
            }
        }

        let control_gain = self
            .control
            .as_ref()
            .map(SparseCueRenderControl::volume_gain)
            .unwrap_or(1.0);

        for (sample, route) in self.input_frame.iter().copied().zip(self.routes.iter()) {
            if !route.active || !route.master_enabled {
                continue;
            }

            let source_gain = route.gain_linear * self.cue_trim * master_gain * control_gain;
            let routed_sample = sample * source_gain;
            if routed_sample == 0.0 {
                continue;
            }

            for destination in &route.destinations {
                let Some(setting) = output_settings.get(destination.output_channel) else {
                    continue;
                };
                if !setting.active {
                    continue;
                }

                output_frame[destination.output_channel] +=
                    routed_sample * destination.cue_output_gain_linear * setting.gain_linear;
            }
        }
    }
}

/// Test-only renderer for one endpoint. It owns a fixed-slot Cue source table
/// and one endpoint-sized PCM frame; each Cue owns only source-channel scratch.
pub(crate) struct SparseRuntimeRenderer {
    ownership: SparseRendererOwnership,
    output_channels: usize,
    sample_rate: u32,
    output_frame: Vec<f32>,
    output_index: usize,
    output_settings: Vec<SparseEndpointOutputSetting>,
    master_gain: f32,
}

impl SparseRuntimeRenderer {
    pub(crate) fn new(
        output_channels: usize,
        sample_rate: u32,
        output_settings: &[OutputChannelSetting],
        master_gain: f32,
        ownership: SparseRendererOwnership,
    ) -> Result<Self, String> {
        if output_channels == 0 || output_channels > u16::MAX as usize {
            return Err(format!("invalid output channel count: {output_channels}"));
        }
        if sample_rate == 0 {
            return Err("sample rate must be greater than zero".to_string());
        }

        let output_settings = (0..output_channels)
            .map(|index| {
                let setting = output_settings.get(index);
                SparseEndpointOutputSetting {
                    active: setting.map(|setting| setting.active).unwrap_or(true),
                    gain_linear: db_to_linear(
                        setting.map(|setting| setting.gain_db).unwrap_or(0.0),
                    ),
                }
            })
            .collect();

        Ok(Self {
            ownership,
            output_channels,
            sample_rate,
            output_frame: vec![0.0; output_channels],
            output_index: output_channels,
            output_settings,
            master_gain,
        })
    }

    /// Prepares a source on the control side. The UniformSourceIterator keeps
    /// the source's original channel count and changes only its sample rate.
    /// This is intentionally different from the legacy path, which expands
    /// each Cue to the full endpoint layout before rate conversion.
    pub(crate) fn prepare_payload<S>(
        &self,
        source: S,
        routes: &[ChannelRoute],
        cue_output_gains_db: &[f32],
        cue_trim: f32,
    ) -> SparseRendererPayload
    where
        S: Source<Item = f32> + Send + 'static,
    {
        self.prepare_payload_inner(source, routes, cue_output_gains_db, cue_trim, None)
    }

    /// Prepares a Cue payload with an atomic control handle. This is the
    /// Stage 4.3a bridge contract: the control thread keeps the handle while
    /// the render slot owns the shared render-side state.
    pub(crate) fn prepare_payload_with_control<S>(
        &self,
        source: S,
        routes: &[ChannelRoute],
        cue_output_gains_db: &[f32],
        cue_trim: f32,
        control: SparseCueRenderControl,
    ) -> SparseRendererPayload
    where
        S: Source<Item = f32> + Send + 'static,
    {
        self.prepare_payload_inner(source, routes, cue_output_gains_db, cue_trim, Some(control))
    }

    fn prepare_payload_inner<S>(
        &self,
        source: S,
        routes: &[ChannelRoute],
        cue_output_gains_db: &[f32],
        cue_trim: f32,
        control: Option<SparseCueRenderControl>,
    ) -> SparseRendererPayload
    where
        S: Source<Item = f32> + Send + 'static,
    {
        let source_channels = source.channels();
        let resources = SparseCueRenderResources::new(
            source_channels.get() as usize,
            self.output_channels,
            routes,
            cue_output_gains_db,
            cue_trim,
            control,
        );
        let endpoint_rate = NonZeroU32::new(self.sample_rate)
            .expect("SparseRuntimeRenderer validates a non-zero sample rate");
        let resampled =
            rodio::source::UniformSourceIterator::new(source, source_channels, endpoint_rate);
        let source: SparseSource = Box::new(resampled);
        CueSourcePayload::new(source, resources)
    }

    /// Installs a fully prepared payload. Ownership-preserving rejection is
    /// delegated to Stage 4.2b2 and returned unchanged to the caller.
    pub(crate) fn add_payload(
        &mut self,
        key: CueKey,
        payload: SparseRendererPayload,
    ) -> Result<SlotToken, AddSourceError<SparseRendererPayload>> {
        self.ownership.add(key, payload)
    }

    pub(crate) fn begin_retire(&mut self, key: CueKey) -> Result<RetireStatus, SlotError> {
        self.ownership.begin_retire(key)
    }

    /// Collects Cue keys whose sources have reached EOF. This is a control-side
    /// scan and must not be called from `render_frame` or an audio callback.
    /// Returns the total number found; if it exceeds `output.len()`, the caller
    /// must provide a larger buffer and scan again before retiring all of them.
    pub(crate) fn collect_finished_cue_keys(&mut self, output: &mut [CueKey]) -> usize {
        let mut found = 0;
        self.ownership
            .for_each_active_payload_mut(|key, _token, payload| {
                if payload.route_resources().is_finished() {
                    if found < output.len() {
                        output[found] = key;
                    }
                    found += 1;
                }
            });
        found
    }

    /// Starts Stop All from a non-render control path. Slots remain occupied
    /// until their exact Reaper acknowledgements are polled.
    pub(crate) fn begin_retire_all(&mut self) -> RetireAllReport {
        self.ownership.begin_retire_all()
    }

    pub(crate) fn retry_pending_retirements(&mut self) -> RetryReport {
        self.ownership.retry_pending_retirements()
    }

    /// Called from a non-render control path. Reaper acknowledgements are
    /// never polled from `next` or `render_frame`.
    pub(crate) fn poll_reaper_acknowledgements(&mut self) -> AckReport {
        self.ownership.poll_reaper_acknowledgements()
    }

    pub(crate) fn slot_token(&self, key: CueKey) -> Option<SlotToken> {
        self.ownership.slot_token(key)
    }

    pub(crate) fn active_count(&self) -> usize {
        self.ownership.active_count()
    }

    pub(crate) fn occupied_count(&self) -> usize {
        self.ownership.occupied_count()
    }

    /// Renders exactly one complete endpoint frame into the preallocated
    /// endpoint buffer. No locks, waits, or explicit heap allocations occur
    /// in this method; retirement and acknowledgement remain control-side work.
    #[inline]
    pub(crate) fn render_frame(&mut self) -> &[f32] {
        self.output_frame.fill(0.0);

        let output_frame = &mut self.output_frame;
        let output_settings = &self.output_settings;
        let master_gain = self.master_gain;
        self.ownership
            .for_each_active_payload_mut(|_key, _token, payload| {
                let (source, resources) = payload.parts_mut();
                resources.mix_next_frame_into(source, output_frame, output_settings, master_gain);
            });
        self.output_index = self.output_channels;

        &self.output_frame
    }
}

impl Iterator for SparseRuntimeRenderer {
    type Item = f32;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.output_index >= self.output_channels {
            self.render_frame();
            self.output_index = 0;
        }

        let sample = self.output_frame[self.output_index];
        self.output_index += 1;
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

impl Source for SparseRuntimeRenderer {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> NonZeroU16 {
        NonZeroU16::new(self.output_channels as u16)
            .expect("SparseRuntimeRenderer validates a non-zero output channel count")
    }

    fn sample_rate(&self) -> NonZeroU32 {
        NonZeroU32::new(self.sample_rate)
            .expect("SparseRuntimeRenderer validates a non-zero sample rate")
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{RetireStatus, SparseRuntimeRenderer};
    use crate::audio::engine::sparse_runtime_contract::MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT;
    use crate::audio::engine::sparse_runtime_contract::{CueId, CueKey};
    use crate::audio::engine::sparse_runtime_ownership::CueSourceOwnership;
    use crate::audio::engine::{
        ChannelRouter, SharedOutputSettings, StopFadeControl, StopFadeSource,
        VolumeAutomationSource, STOP_DECLICK_FADE_MS,
    };
    use crate::cue::{ChannelRoute, OutputChannelSetting};
    use crossbeam_channel::{unbounded, Receiver, Sender};
    use parking_lot::RwLock;
    use rodio::buffer::SamplesBuffer;
    use rodio::Source;
    use std::num::{NonZeroU16, NonZeroU32};
    use std::sync::atomic::AtomicU64;
    use std::sync::mpsc::{self, Receiver as MpscReceiver, Sender as MpscSender};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    const ENDPOINT_CHANNELS: usize = 128;
    const ENDPOINT_RATE: u32 = 48_000;
    const AUTOMATION_POSITION_SECS: f64 = 0.5;
    const AUTOMATION_POINTS: [[f64; 2]; 2] = [[0.0, 0.9], [1.0, 0.3]];

    #[derive(Clone)]
    struct CueSpec {
        source_channels: usize,
        source_rate: u32,
        samples: Vec<f32>,
        routes: Vec<ChannelRoute>,
        cue_output_gains_db: Vec<f32>,
        cue_trim: f32,
    }

    struct GainSource<S> {
        inner: S,
        gain: f32,
    }

    impl<S> GainSource<S> {
        fn new(inner: S, gain: f32) -> Self {
            Self { inner, gain }
        }
    }

    impl<S> Iterator for GainSource<S>
    where
        S: Source<Item = f32>,
    {
        type Item = f32;

        fn next(&mut self) -> Option<Self::Item> {
            self.inner.next().map(|sample| sample * self.gain)
        }
    }

    impl<S> Source for GainSource<S>
    where
        S: Source<Item = f32>,
    {
        fn current_span_len(&self) -> Option<usize> {
            self.inner.current_span_len()
        }

        fn channels(&self) -> NonZeroU16 {
            self.inner.channels()
        }

        fn sample_rate(&self) -> NonZeroU32 {
            self.inner.sample_rate()
        }

        fn total_duration(&self) -> Option<Duration> {
            self.inner.total_duration()
        }
    }

    fn key(id: u128) -> CueKey {
        key_with_generation(id, 1)
    }

    fn key_with_generation(id: u128, generation: u64) -> CueKey {
        CueKey {
            id: CueId(id),
            generation,
        }
    }

    struct BlockingDropSource<S> {
        inner: S,
        started: Option<MpscSender<()>>,
        release: Option<MpscReceiver<()>>,
        dropped_on: MpscSender<thread::ThreadId>,
    }

    impl<S> Iterator for BlockingDropSource<S>
    where
        S: Source<Item = f32>,
    {
        type Item = f32;

        fn next(&mut self) -> Option<Self::Item> {
            self.inner.next()
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            self.inner.size_hint()
        }
    }

    impl<S> Source for BlockingDropSource<S>
    where
        S: Source<Item = f32>,
    {
        fn current_span_len(&self) -> Option<usize> {
            self.inner.current_span_len()
        }

        fn channels(&self) -> NonZeroU16 {
            self.inner.channels()
        }

        fn sample_rate(&self) -> NonZeroU32 {
            self.inner.sample_rate()
        }

        fn total_duration(&self) -> Option<Duration> {
            self.inner.total_duration()
        }
    }

    impl<S> Drop for BlockingDropSource<S> {
        fn drop(&mut self) {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
            }
            if let Some(release) = self.release.take() {
                let _ = release.recv_timeout(Duration::from_secs(2));
            }
            let _ = self.dropped_on.send(thread::current().id());
        }
    }

    fn deterministic_samples(cue_index: usize, source_channels: usize, frames: usize) -> Vec<f32> {
        let mut samples = Vec::with_capacity(source_channels * frames);
        for frame in 0..frames {
            for source_channel in 0..source_channels {
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

    fn make_routes(
        source_channels: usize,
        output_channels: usize,
        fanout: usize,
        seed: usize,
    ) -> Vec<ChannelRoute> {
        (0..source_channels)
            .map(|source_channel| {
                let destinations = (0..fanout)
                    .map(|offset| (seed * 13 + source_channel * 19 + offset) % output_channels)
                    .collect::<Vec<_>>();
                ChannelRoute {
                    active: true,
                    master_enabled: true,
                    output_channel: destinations[0],
                    output_channels: destinations,
                    gain_db: -3.0 + source_channel as f32 * 0.35,
                }
            })
            .collect()
    }

    fn make_spec(
        cue_index: usize,
        source_channels: usize,
        source_rate: u32,
        fanout: usize,
    ) -> CueSpec {
        let routes = make_routes(source_channels, ENDPOINT_CHANNELS, fanout, cue_index);
        let cue_output_gains_db = (0..ENDPOINT_CHANNELS)
            .map(|index| -((index % 7) as f32) * 0.25)
            .collect();
        CueSpec {
            source_channels,
            source_rate,
            samples: deterministic_samples(cue_index, source_channels, 1024),
            routes,
            cue_output_gains_db,
            cue_trim: 0.73 + cue_index as f32 * 0.01,
        }
    }

    fn output_settings() -> Vec<OutputChannelSetting> {
        (0..ENDPOINT_CHANNELS)
            .map(|index| OutputChannelSetting {
                active: index % 17 != 0,
                gain_db: -((index % 5) as f32) * 0.5,
            })
            .collect()
    }

    fn automation_source(
        spec: &CueSpec,
        retired_points_tx: &Sender<Vec<[f64; 2]>>,
    ) -> VolumeAutomationSource<SamplesBuffer> {
        let source = SamplesBuffer::new(
            NonZeroU16::new(spec.source_channels as u16).unwrap(),
            NonZeroU32::new(spec.source_rate).unwrap(),
            spec.samples.clone(),
        );
        let (updates_tx, updates_rx) = unbounded();
        drop(updates_tx);
        VolumeAutomationSource::new(
            source,
            true,
            AUTOMATION_POINTS.to_vec(),
            updates_rx,
            retired_points_tx.clone(),
            Arc::new(AtomicU64::new(AUTOMATION_POSITION_SECS.to_bits())),
        )
    }

    fn render_legacy_reference(
        specs: &[CueSpec],
        frames: usize,
        settings: &[OutputChannelSetting],
        master_gain: f32,
        retired_points_tx: &Sender<Vec<[f64; 2]>>,
    ) -> Vec<f32> {
        let channels = NonZeroU16::new(ENDPOINT_CHANNELS as u16).unwrap();
        let sample_rate = NonZeroU32::new(ENDPOINT_RATE).unwrap();
        let (mixer, mut output) = rodio::mixer::mixer(channels, sample_rate);
        let shared_settings: SharedOutputSettings = Arc::new(RwLock::new(settings.to_vec()));

        for spec in specs {
            let automated = automation_source(spec, retired_points_tx);
            let routed = ChannelRouter::new(
                automated,
                Arc::new(RwLock::new(spec.routes.clone())),
                shared_settings.clone(),
                Arc::new(RwLock::new(spec.cue_output_gains_db.clone())),
                ENDPOINT_CHANNELS,
            );
            let endpoint_source =
                rodio::source::UniformSourceIterator::new(routed, channels, sample_rate);
            mixer.add(GainSource::new(
                endpoint_source,
                spec.cue_trim * master_gain,
            ));
        }

        let mut rendered = Vec::with_capacity(frames * ENDPOINT_CHANNELS);
        for _ in 0..frames * ENDPOINT_CHANNELS {
            rendered.push(
                output
                    .next()
                    .expect("legacy Rodio mixer must render audio or silence"),
            );
        }
        drop(output);
        drop(mixer);
        rendered
    }

    fn render_sparse(
        specs: &[CueSpec],
        frames: usize,
        settings: &[OutputChannelSetting],
        master_gain: f32,
        retired_points_tx: &Sender<Vec<[f64; 2]>>,
    ) -> Vec<f32> {
        let (ownership, reaper) = CueSourceOwnership::spawn(256).unwrap();
        let mut renderer = SparseRuntimeRenderer::new(
            ENDPOINT_CHANNELS,
            ENDPOINT_RATE,
            settings,
            master_gain,
            ownership,
        )
        .unwrap();

        let mut keys = Vec::with_capacity(specs.len());
        for (index, spec) in specs.iter().enumerate() {
            let source = automation_source(spec, retired_points_tx);
            let payload = renderer.prepare_payload(
                source,
                &spec.routes,
                &spec.cue_output_gains_db,
                spec.cue_trim,
            );
            let cue_key = key(index as u128 + 1);
            renderer.add_payload(cue_key, payload).unwrap();
            keys.push(cue_key);
        }

        let mut rendered = Vec::with_capacity(frames * ENDPOINT_CHANNELS);
        for _ in 0..frames * ENDPOINT_CHANNELS {
            rendered.push(
                renderer
                    .next()
                    .expect("sparse runtime renderer must produce audio or silence"),
            );
        }

        for cue_key in keys {
            renderer.begin_retire(cue_key).unwrap();
        }
        wait_for_acknowledgements(&mut renderer, specs.len());

        drop(renderer);
        let stats = reaper.join().unwrap();
        assert_eq!(stats.retired_payloads, specs.len());
        rendered
    }

    // Keep the receiver alive so automation point arrays are transferred
    // away from the Reaper/reference teardown paths rather than leaked.
    fn retired_points_channel() -> (Sender<Vec<[f64; 2]>>, Receiver<Vec<[f64; 2]>>) {
        unbounded()
    }

    fn wait_for_acknowledgements(renderer: &mut SparseRuntimeRenderer, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut acknowledged = 0;
        while acknowledged < expected {
            let _retry_report = renderer.retry_pending_retirements();
            acknowledged += renderer.poll_reaper_acknowledgements().acknowledged;
            assert!(
                Instant::now() < deadline,
                "timed out waiting for Reaper acknowledgements"
            );
            if acknowledged < expected {
                thread::yield_now();
            }
        }
        assert_eq!(renderer.active_count(), 0);
        assert_eq!(renderer.occupied_count(), 0);
    }

    fn assert_sample_parity(label: &str, expected: &[f32], actual: &[f32]) {
        assert_eq!(
            expected.len(),
            actual.len(),
            "{label}: sample count differs"
        );
        let mut max_error = 0.0_f32;
        for (index, (&expected_sample, &actual_sample)) in
            expected.iter().zip(actual.iter()).enumerate()
        {
            let error = (expected_sample - actual_sample).abs();
            max_error = max_error.max(error);
            assert!(
                error <= 2.0e-5,
                "{label}: mismatch at interleaved sample {index} \
                 (frame {}, output {}): expected {expected_sample:?}, \
                 actual {actual_sample:?}, error={error}",
                index / ENDPOINT_CHANNELS,
                index % ENDPOINT_CHANNELS,
            );
        }
        assert!(max_error <= 2.0e-5, "{label}: max error {max_error}");
    }

    #[test]
    fn mono_fanout_supports_16_32_64_and_128_destinations() {
        for fanout in [16, 32, 64, 128] {
            let (ownership, reaper) = CueSourceOwnership::spawn(256).unwrap();
            let mut renderer =
                SparseRuntimeRenderer::new(ENDPOINT_CHANNELS, ENDPOINT_RATE, &[], 1.0, ownership)
                    .unwrap();
            let source = SamplesBuffer::new(
                NonZeroU16::new(1).unwrap(),
                NonZeroU32::new(ENDPOINT_RATE).unwrap(),
                vec![0.25; 2],
            );
            let route = ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: 0,
                output_channels: (0..fanout).collect(),
                gain_db: 0.0,
            };
            let payload = renderer.prepare_payload(source, &[route], &[], 1.0);
            let cue_key = key(fanout as u128);
            renderer.add_payload(cue_key, payload).unwrap();

            let frame = renderer.render_frame();
            assert!(frame[..fanout].iter().all(|sample| *sample == 0.25));
            assert!(frame[fanout..].iter().all(|sample| *sample == 0.0));

            renderer.begin_retire(cue_key).unwrap();
            wait_for_acknowledgements(&mut renderer, 1);
            drop(renderer);
            assert_eq!(reaper.join().unwrap().retired_payloads, 1);
        }
    }

    #[test]
    fn six_channel_source_uses_source_sized_scratch_for_128_output_endpoint() {
        let (ownership, reaper) = CueSourceOwnership::spawn(256).unwrap();
        let mut renderer =
            SparseRuntimeRenderer::new(ENDPOINT_CHANNELS, ENDPOINT_RATE, &[], 1.0, ownership)
                .unwrap();
        let input = vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
        let source = SamplesBuffer::new(
            NonZeroU16::new(6).unwrap(),
            NonZeroU32::new(ENDPOINT_RATE).unwrap(),
            input.clone(),
        );
        let routes = (0..6)
            .map(|source_channel| ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: 122 + source_channel,
                output_channels: vec![122 + source_channel],
                gain_db: 0.0,
            })
            .collect::<Vec<_>>();
        let payload = renderer.prepare_payload(source, &routes, &[], 1.0);
        assert_eq!(payload.route_resources().input_frame.len(), 6);
        let cue_key = key(600);
        renderer.add_payload(cue_key, payload).unwrap();

        assert_eq!(renderer.output_frame.len(), ENDPOINT_CHANNELS);
        let frame = renderer.render_frame();
        for (offset, expected) in input.iter().enumerate() {
            assert_eq!(frame[122 + offset], *expected);
        }
        assert!(frame[..122].iter().all(|sample| *sample == 0.0));

        renderer.begin_retire(cue_key).unwrap();
        wait_for_acknowledgements(&mut renderer, 1);
        drop(renderer);
        assert_eq!(reaper.join().unwrap().retired_payloads, 1);
    }

    #[test]
    fn sample_rate_conversion_and_processing_match_legacy_reference_for_mono_stereo_and_multichannel(
    ) {
        let settings = output_settings();
        let master_gain = 0.61;
        for (source_channels, source_rate, fanout) in
            [(1, 44_100, 32), (2, 48_000, 64), (6, 96_000, 128)]
        {
            let spec = make_spec(3, source_channels, source_rate, fanout);
            let (retired_tx, _retired_rx) = retired_points_channel();
            let expected = render_legacy_reference(
                std::slice::from_ref(&spec),
                192,
                &settings,
                master_gain,
                &retired_tx,
            );
            let actual = render_sparse(
                std::slice::from_ref(&spec),
                192,
                &settings,
                master_gain,
                &retired_tx,
            );
            assert_sample_parity(
                &format!("{source_channels}ch source at {source_rate} Hz, {fanout} destinations"),
                &expected,
                &actual,
            );
        }
    }

    #[test]
    fn fifteen_cues_sum_sample_by_sample_on_shared_outputs() {
        let settings = output_settings();
        let master_gain = 0.58;
        let routes = {
            let destinations = (0..16).collect::<Vec<_>>();
            vec![
                ChannelRoute {
                    active: true,
                    master_enabled: true,
                    output_channel: 0,
                    output_channels: destinations.clone(),
                    gain_db: -2.0,
                },
                ChannelRoute {
                    active: true,
                    master_enabled: true,
                    output_channel: 0,
                    output_channels: destinations,
                    gain_db: -1.0,
                },
            ]
        };
        let specs = (0..15)
            .map(|index| {
                let mut spec = make_spec(index, 2, 48_000, 16);
                spec.routes = routes.clone();
                spec
            })
            .collect::<Vec<_>>();
        let (retired_tx, _retired_rx) = retired_points_channel();
        let expected = render_legacy_reference(&specs, 257, &settings, master_gain, &retired_tx);
        let actual = render_sparse(&specs, 257, &settings, master_gain, &retired_tx);
        assert_sample_parity("15 stereo Cues / 16 shared outputs", &expected, &actual);
    }

    #[test]
    fn retiring_cue_is_removed_from_render_set_before_reaper_acknowledgement() {
        let (ownership, reaper) = CueSourceOwnership::spawn(256).unwrap();
        let mut renderer =
            SparseRuntimeRenderer::new(ENDPOINT_CHANNELS, ENDPOINT_RATE, &[], 1.0, ownership)
                .unwrap();
        let source = SamplesBuffer::new(
            NonZeroU16::new(1).unwrap(),
            NonZeroU32::new(ENDPOINT_RATE).unwrap(),
            vec![0.5; 32],
        );
        let route = ChannelRoute::default();
        let payload = renderer.prepare_payload(source, &[route], &[], 1.0);
        let cue_key = key(900);
        renderer.add_payload(cue_key, payload).unwrap();

        assert!(renderer.render_frame()[0] > 0.0);
        renderer.begin_retire(cue_key).unwrap();
        assert_eq!(renderer.active_count(), 0);
        assert!(renderer.render_frame().iter().all(|sample| *sample == 0.0));

        wait_for_acknowledgements(&mut renderer, 1);
        drop(renderer);
        assert_eq!(reaper.join().unwrap().retired_payloads, 1);
    }

    #[test]
    fn natural_eof_is_collected_on_control_side_before_reaper_retirement() {
        let (ownership, reaper) = CueSourceOwnership::spawn(4).unwrap();
        let mut renderer =
            SparseRuntimeRenderer::new(2, ENDPOINT_RATE, &[], 1.0, ownership).unwrap();
        let cue_key = key(1_001);
        let source = SamplesBuffer::new(
            NonZeroU16::new(2).unwrap(),
            NonZeroU32::new(ENDPOINT_RATE).unwrap(),
            vec![0.1, 0.2, 0.3, 0.4],
        );
        let routes = vec![
            ChannelRoute::default(),
            ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: 1,
                output_channels: vec![1],
                gain_db: 0.0,
            },
        ];
        let payload = renderer.prepare_payload(source, &routes, &[], 1.0);
        renderer.add_payload(cue_key, payload).unwrap();

        let first_frame = renderer.render_frame();
        assert!((first_frame[0] - 0.1).abs() <= 1.0e-6);
        assert!((first_frame[1] - 0.2).abs() <= 1.0e-6);
        let second_frame = renderer.render_frame();
        assert!((second_frame[0] - 0.3).abs() <= 1.0e-6);
        assert!((second_frame[1] - 0.4).abs() <= 1.0e-6);
        // EOF is observed on the next complete-frame request. The source
        // remains active until the control side explicitly retires it.
        assert_eq!(renderer.render_frame(), &[0.0, 0.0]);
        assert_eq!(renderer.active_count(), 1);

        let mut finished = [key(0); MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT];
        let found = renderer.collect_finished_cue_keys(&mut finished);
        assert_eq!(found, 1);
        assert_eq!(finished[0], cue_key);

        // Detection is separate from rendering; only the control side retires.
        assert_eq!(renderer.active_count(), 1);
        assert_eq!(renderer.begin_retire(cue_key), Ok(RetireStatus::Queued));
        wait_for_acknowledgements(&mut renderer, 1);
        drop(renderer);
        assert_eq!(reaper.join().unwrap().retired_payloads, 1);
    }

    #[test]
    fn stop_fade_reaches_zero_then_natural_completion_is_reaped() {
        let (ownership, reaper) = CueSourceOwnership::spawn(4).unwrap();
        let mut renderer =
            SparseRuntimeRenderer::new(1, ENDPOINT_RATE, &[], 1.0, ownership).unwrap();
        let cue_key = key(1_002);
        let control = StopFadeControl::new();
        let source = SamplesBuffer::new(
            NonZeroU16::new(1).unwrap(),
            NonZeroU32::new(ENDPOINT_RATE).unwrap(),
            vec![0.75; 320],
        );
        let stop_fade = StopFadeSource::new(source, control.clone());
        let payload = renderer.prepare_payload(stop_fade, &[ChannelRoute::default()], &[], 1.0);
        renderer.add_payload(cue_key, payload).unwrap();

        assert!((renderer.render_frame()[0] - 0.75).abs() <= 1.0e-6);
        control.request();

        let fade_frames = (ENDPOINT_RATE as f64 * STOP_DECLICK_FADE_MS / 1_000.0).round() as usize;
        let mut fade_samples = Vec::with_capacity(fade_frames);
        for _ in 0..fade_frames {
            fade_samples.push(renderer.render_frame()[0]);
        }

        assert!((fade_samples[0] - 0.75).abs() <= 1.0e-6);
        assert!(fade_samples[fade_frames - 1].abs() <= 1.0e-6);
        assert!(fade_samples.iter().all(|sample| *sample >= -1.0e-6));
        assert!(fade_samples
            .windows(2)
            .all(|pair| pair[1] <= pair[0] + 1.0e-6));

        assert_eq!(renderer.render_frame(), &[0.0]);
        assert!(control.is_completed());

        let mut finished = [key(0); MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT];
        assert_eq!(renderer.collect_finished_cue_keys(&mut finished), 1);
        assert_eq!(finished[0], cue_key);
        assert_eq!(renderer.begin_retire(cue_key), Ok(RetireStatus::Queued));
        wait_for_acknowledgements(&mut renderer, 1);
        drop(renderer);
        assert_eq!(reaper.join().unwrap().retired_payloads, 1);
    }

    #[test]
    fn stop_all_retires_256_active_cues_and_waits_for_exact_acknowledgements() {
        let (ownership, reaper) = CueSourceOwnership::spawn(256).unwrap();
        let mut renderer =
            SparseRuntimeRenderer::new(1, ENDPOINT_RATE, &[], 1.0, ownership).unwrap();

        let mut expected_sum = 0.0_f32;
        for id in 1..=MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT {
            let value = id as f32 / 100_000.0;
            expected_sum += value;
            let source = SamplesBuffer::new(
                NonZeroU16::new(1).unwrap(),
                NonZeroU32::new(ENDPOINT_RATE).unwrap(),
                vec![value; 8],
            );
            let payload = renderer.prepare_payload(source, &[ChannelRoute::default()], &[], 1.0);
            renderer.add_payload(key(id as u128), payload).unwrap();
        }

        assert_eq!(renderer.active_count(), 256);
        assert_eq!(renderer.occupied_count(), 256);
        assert!((renderer.render_frame()[0] - expected_sum).abs() <= 1.0e-5);

        let report = renderer.begin_retire_all();
        assert_eq!(report.attempted, 256);
        assert_eq!(report.queued, 256);
        assert_eq!(report.queue_full, 0);
        assert_eq!(report.disconnected, 0);
        assert_eq!(renderer.active_count(), 0);
        assert_eq!(renderer.occupied_count(), 256);
        assert!(renderer.render_frame().iter().all(|sample| *sample == 0.0));

        wait_for_acknowledgements(&mut renderer, 256);
        drop(renderer);
        assert_eq!(reaper.join().unwrap().retired_payloads, 256);
    }

    #[test]
    fn rapid_restart_keeps_new_generation_active_while_old_source_retires() {
        let (ownership, reaper) = CueSourceOwnership::spawn(4).unwrap();
        let worker_id = reaper.thread_id();
        let mut renderer =
            SparseRuntimeRenderer::new(1, ENDPOINT_RATE, &[], 1.0, ownership).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (dropped_tx, dropped_rx) = mpsc::channel();

        let old_key = key_with_generation(1_003, 1);
        let old_source = BlockingDropSource {
            inner: SamplesBuffer::new(
                NonZeroU16::new(1).unwrap(),
                NonZeroU32::new(ENDPOINT_RATE).unwrap(),
                vec![0.25; 16],
            ),
            started: Some(started_tx),
            release: Some(release_rx),
            dropped_on: dropped_tx.clone(),
        };
        let old_payload =
            renderer.prepare_payload(old_source, &[ChannelRoute::default()], &[], 1.0);
        let old_token = renderer.add_payload(old_key, old_payload).unwrap();
        assert!((renderer.render_frame()[0] - 0.25).abs() <= 1.0e-6);

        assert_eq!(renderer.begin_retire(old_key), Ok(RetireStatus::Queued));
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("old source destructor should be running on Reaper");

        let new_key = key_with_generation(1_003, 2);
        let new_source = SamplesBuffer::new(
            NonZeroU16::new(1).unwrap(),
            NonZeroU32::new(ENDPOINT_RATE).unwrap(),
            vec![0.75; 16],
        );
        let new_payload =
            renderer.prepare_payload(new_source, &[ChannelRoute::default()], &[], 1.0);
        let new_token = renderer.add_payload(new_key, new_payload).unwrap();
        assert_ne!(old_token.index, new_token.index);
        assert_eq!(renderer.active_count(), 1);
        assert_eq!(renderer.occupied_count(), 2);
        assert!((renderer.render_frame()[0] - 0.75).abs() <= 1.0e-6);

        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let report = renderer.poll_reaper_acknowledgements();
            if report.acknowledged == 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "old generation Reaper acknowledgement did not arrive"
            );
            thread::yield_now();
        }
        assert_eq!(
            dropped_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            worker_id
        );
        assert_eq!(renderer.active_count(), 1);
        assert_eq!(renderer.occupied_count(), 1);
        assert_eq!(renderer.slot_token(new_key), Some(new_token));

        assert_eq!(renderer.begin_retire(new_key), Ok(RetireStatus::Queued));
        wait_for_acknowledgements(&mut renderer, 1);
        drop(renderer);
        assert_eq!(reaper.join().unwrap().retired_payloads, 2);
    }
}
