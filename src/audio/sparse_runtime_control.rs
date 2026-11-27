//! Stage 4.3a test-only Cue control bridge for the Sparse Runtime.
//!
//! The handle mirrors the small control surface currently used from
//! `ActivePlayer` (pause, play, set_volume, stop, is_paused, empty) while
//! publishing state through atomics. The renderer samples that state once per
//! complete endpoint frame. It never acquires a mutex or owns control-side
//! resources.
//!
//! This is deliberately test-only. Production Audio Worker integration is a
//! later stage after this control contract is validated on Windows.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

struct SparseCueControlState {
    paused: AtomicBool,
    hard_stop_requested: AtomicBool,
    volume_bits: AtomicU32,
    finished: AtomicBool,
}

/// Control-thread handle. Cloning it does not duplicate the playback state.
#[derive(Clone)]
pub(crate) struct SparseCueControlHandle {
    state: Arc<SparseCueControlState>,
}

/// Render-side view of the same state. It contains only atomic accessors and
/// is owned by the Cue's preallocated render resources.
#[derive(Clone)]
pub(crate) struct SparseCueRenderControl {
    state: Arc<SparseCueControlState>,
}

impl SparseCueControlHandle {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(SparseCueControlState {
                paused: AtomicBool::new(false),
                hard_stop_requested: AtomicBool::new(false),
                volume_bits: AtomicU32::new(1.0_f32.to_bits()),
                finished: AtomicBool::new(false),
            }),
        }
    }

    pub(crate) fn render_control(&self) -> SparseCueRenderControl {
        SparseCueRenderControl {
            state: Arc::clone(&self.state),
        }
    }

    /// Mirrors `rodio::Player::pause`.
    #[inline]
    pub(crate) fn pause(&self) {
        self.state.paused.store(true, Ordering::Release);
    }

    /// Mirrors `rodio::Player::play`.
    #[inline]
    pub(crate) fn play(&self) {
        if !self.state.finished.load(Ordering::Acquire) {
            self.state.paused.store(false, Ordering::Release);
        }
    }

    #[inline]
    pub(crate) fn is_paused(&self) -> bool {
        self.state.paused.load(Ordering::Acquire)
    }

    /// Mirrors `rodio::Player::set_volume`. The value is sampled once per
    /// endpoint frame; non-finite values are rejected as silence-safe zero.
    #[inline]
    pub(crate) fn set_volume(&self, volume: f32) {
        let volume = if volume.is_finite() { volume } else { 0.0 };
        self.state
            .volume_bits
            .store(volume.to_bits(), Ordering::Release);
    }

    #[inline]
    pub(crate) fn volume(&self) -> f32 {
        f32::from_bits(self.state.volume_bits.load(Ordering::Acquire))
    }

    /// Requests an immediate stop. The render side acknowledges it at the
    /// next complete endpoint-frame boundary without dropping the Cue source.
    #[inline]
    pub(crate) fn stop(&self) {
        self.state
            .hard_stop_requested
            .store(true, Ordering::Release);
    }

    /// Used when the endpoint is already paused and no render frame can arrive.
    /// The control side may then retire the Cue without resuming the endpoint.
    #[inline]
    pub(crate) fn complete_stop_without_render(&self) {
        self.state
            .hard_stop_requested
            .store(true, Ordering::Release);
        self.state.finished.store(true, Ordering::Release);
    }

    /// Mirrors `rodio::Player::empty`, but becomes true only after the renderer
    /// has observed EOF/stop (or the control side completed a paused stop).
    #[inline]
    pub(crate) fn empty(&self) -> bool {
        self.state.finished.load(Ordering::Acquire)
    }
}

impl SparseCueRenderControl {
    /// Returns false while paused or after a stop request. A hard stop is
    /// acknowledged here, but the source remains owned by the runtime slot
    /// until the control side transfers the payload to the Reaper.
    #[inline]
    pub(crate) fn begin_endpoint_frame(&self) -> bool {
        if self.state.finished.load(Ordering::Acquire) {
            return false;
        }

        if self.state.hard_stop_requested.load(Ordering::Acquire) {
            self.state.finished.store(true, Ordering::Release);
            return false;
        }

        !self.state.paused.load(Ordering::Acquire)
    }

    #[inline]
    pub(crate) fn volume_gain(&self) -> f32 {
        f32::from_bits(self.state.volume_bits.load(Ordering::Acquire))
    }

    #[inline]
    pub(crate) fn mark_finished(&self) {
        self.state.finished.store(true, Ordering::Release);
    }

    #[inline]
    pub(crate) fn is_finished(&self) -> bool {
        self.state.finished.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::SparseCueControlHandle;
    use crate::audio::engine::sparse_runtime_contract::{CueId, CueKey};
    use crate::audio::engine::sparse_runtime_ownership::CueSourceOwnership;
    use crate::audio::engine::sparse_runtime_renderer::SparseRuntimeRenderer;
    use crate::audio::engine::{StopFadeControl, StopFadeSource, STOP_DECLICK_FADE_MS};
    use crate::cue::{ChannelRoute, OutputChannelSetting};
    use rodio::buffer::SamplesBuffer;
    use rodio::Source;
    use std::num::{NonZeroU16, NonZeroU32};
    use std::sync::mpsc::{self, Sender, TryRecvError};
    use std::thread;
    use std::time::{Duration, Instant};

    const SAMPLE_RATE: u32 = 48_000;

    fn key(id: u128) -> CueKey {
        CueKey {
            id: CueId(id),
            generation: 1,
        }
    }

    fn stereo_routes() -> Vec<ChannelRoute> {
        vec![
            ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: 0,
                output_channels: vec![0],
                gain_db: 0.0,
            },
            ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: 1,
                output_channels: vec![1],
                gain_db: 0.0,
            },
        ]
    }

    fn wait_for_acknowledgements(renderer: &mut SparseRuntimeRenderer, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut acknowledged = 0;
        while acknowledged < expected {
            let _ = renderer.retry_pending_retirements();
            acknowledged += renderer.poll_reaper_acknowledgements().acknowledged;
            assert!(
                Instant::now() < deadline,
                "timed out waiting for Sparse Runtime Reaper acknowledgements"
            );
            if acknowledged < expected {
                thread::yield_now();
            }
        }
        assert_eq!(renderer.active_count(), 0);
        assert_eq!(renderer.occupied_count(), 0);
    }

    fn make_renderer(
        output_channels: usize,
    ) -> (
        SparseRuntimeRenderer,
        crate::audio::engine::sparse_runtime_reaper::ReaperWorker,
    ) {
        let (ownership, reaper) = CueSourceOwnership::spawn(8).unwrap();
        let renderer = SparseRuntimeRenderer::new(
            output_channels,
            SAMPLE_RATE,
            &vec![OutputChannelSetting::default(); output_channels],
            1.0,
            ownership,
        )
        .unwrap();
        (renderer, reaper)
    }

    #[test]
    fn pause_resume_and_live_volume_use_the_same_sparse_cue_control_handle() {
        let (mut renderer, reaper) = make_renderer(2);
        let control = SparseCueControlHandle::new();
        let source = SamplesBuffer::new(
            NonZeroU16::new(2).unwrap(),
            NonZeroU32::new(SAMPLE_RATE).unwrap(),
            vec![0.2_f32, 0.4, 0.3, 0.5, 0.4, 0.6],
        );
        let payload = renderer.prepare_payload_with_control(
            source,
            &stereo_routes(),
            &[],
            1.0,
            control.render_control(),
        );
        renderer.add_payload(key(1), payload).unwrap();

        assert_eq!(renderer.render_frame(), &[0.2, 0.4]);

        control.pause();
        assert!(control.is_paused());
        assert_eq!(renderer.render_frame(), &[0.0, 0.0]);
        assert!(!control.empty(), "pause must not mark the Cue as finished");

        control.play();
        assert!(!control.is_paused());
        assert_eq!(renderer.render_frame(), &[0.3, 0.5]);

        control.set_volume(0.5);
        assert!((control.volume() - 0.5).abs() < f32::EPSILON);
        assert_eq!(renderer.render_frame(), &[0.2, 0.3]);

        // EOF is acknowledged by the renderer at the next full-frame request.
        assert_eq!(renderer.render_frame(), &[0.0, 0.0]);
        assert!(control.empty());

        renderer.begin_retire(key(1)).unwrap();
        wait_for_acknowledgements(&mut renderer, 1);
        drop(renderer);
        assert_eq!(reaper.join().unwrap().retired_payloads, 1);
    }

    struct DropThreadProbe<S> {
        inner: S,
        dropped_on: Sender<thread::ThreadId>,
    }

    impl<S> Iterator for DropThreadProbe<S>
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

    impl<S> Source for DropThreadProbe<S>
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

    impl<S> Drop for DropThreadProbe<S> {
        fn drop(&mut self) {
            let _ = self.dropped_on.send(thread::current().id());
        }
    }

    #[test]
    fn hard_stop_acknowledges_without_dropping_the_source_on_the_render_thread() {
        let (mut renderer, reaper) = make_renderer(1);
        let reaper_thread_id = reaper.thread_id();
        let control = SparseCueControlHandle::new();
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let source = DropThreadProbe {
            inner: SamplesBuffer::new(
                NonZeroU16::new(1).unwrap(),
                NonZeroU32::new(SAMPLE_RATE).unwrap(),
                vec![0.75_f32; 16],
            ),
            dropped_on: dropped_tx,
        };
        let payload = renderer.prepare_payload_with_control(
            source,
            &[ChannelRoute::default()],
            &[],
            1.0,
            control.render_control(),
        );
        renderer.add_payload(key(2), payload).unwrap();

        assert_eq!(renderer.render_frame(), &[0.75]);
        control.stop();
        assert_eq!(renderer.render_frame(), &[0.0]);
        assert!(control.empty());

        assert_eq!(
            dropped_rx.try_recv(),
            Err(TryRecvError::Empty),
            "render-side stop acknowledgement must retain source ownership"
        );

        renderer.begin_retire(key(2)).unwrap();
        wait_for_acknowledgements(&mut renderer, 1);
        assert_eq!(
            dropped_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            reaper_thread_id,
            "Cue source must be destroyed by the Reaper"
        );

        drop(renderer);
        assert_eq!(reaper.join().unwrap().retired_payloads, 1);
    }

    #[test]
    fn paused_stop_can_complete_without_advancing_or_dropping_the_source() {
        let (mut renderer, reaper) = make_renderer(1);
        let reaper_thread_id = reaper.thread_id();
        let control = SparseCueControlHandle::new();
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let source = DropThreadProbe {
            inner: SamplesBuffer::new(
                NonZeroU16::new(1).unwrap(),
                NonZeroU32::new(SAMPLE_RATE).unwrap(),
                vec![0.5_f32; 16],
            ),
            dropped_on: dropped_tx,
        };
        let payload = renderer.prepare_payload_with_control(
            source,
            &[ChannelRoute::default()],
            &[],
            1.0,
            control.render_control(),
        );
        renderer.add_payload(key(3), payload).unwrap();

        assert_eq!(renderer.render_frame(), &[0.5]);
        control.pause();
        assert_eq!(renderer.render_frame(), &[0.0]);
        control.complete_stop_without_render();
        assert!(control.empty());
        let mut finished = [key(0)];
        assert_eq!(renderer.collect_finished_cue_keys(&mut finished), 1);
        assert_eq!(finished[0], key(3));
        assert_eq!(dropped_rx.try_recv(), Err(TryRecvError::Empty));

        renderer.begin_retire(key(3)).unwrap();
        wait_for_acknowledgements(&mut renderer, 1);
        assert_eq!(
            dropped_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            reaper_thread_id
        );

        drop(renderer);
        assert_eq!(reaper.join().unwrap().retired_payloads, 1);
    }

    #[test]
    fn stop_fade_completion_is_published_through_the_control_handle() {
        let (mut renderer, reaper) = make_renderer(1);
        let control = SparseCueControlHandle::new();
        let stop_fade = StopFadeControl::new();
        let source = SamplesBuffer::new(
            NonZeroU16::new(1).unwrap(),
            NonZeroU32::new(SAMPLE_RATE).unwrap(),
            vec![0.75_f32; 1_024],
        );
        let source = StopFadeSource::new(source, stop_fade.clone());
        let payload = renderer.prepare_payload_with_control(
            source,
            &[ChannelRoute::default()],
            &[],
            1.0,
            control.render_control(),
        );
        renderer.add_payload(key(4), payload).unwrap();

        assert_eq!(renderer.render_frame(), &[0.75]);
        stop_fade.request();

        let fade_frames = (SAMPLE_RATE as f64 * STOP_DECLICK_FADE_MS / 1_000.0).round() as usize;
        for _ in 0..fade_frames {
            renderer.render_frame();
        }
        assert_eq!(renderer.render_frame(), &[0.0]);
        assert!(stop_fade.is_completed());
        assert!(control.empty());

        renderer.begin_retire(key(4)).unwrap();
        wait_for_acknowledgements(&mut renderer, 1);
        drop(renderer);
        assert_eq!(reaper.join().unwrap().retired_payloads, 1);
    }
}
