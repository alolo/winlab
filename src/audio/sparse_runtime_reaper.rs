//! Stage 4.2b1 test-only bounded Reaper handoff primitive.
//!
//! The queue transfers ownership of retired resources to a dedicated worker.
//! `try_retire` never waits for queue capacity and returns the original payload
//! on both saturation and disconnection so the caller can retain it and retry.
//! The receiver is owned exclusively by the worker after `spawn` returns.
//!
//! This module is included only under `#[cfg(test)]`. It does not participate
//! in production playback or own live Rodio sources yet.

use super::sparse_runtime_contract::RETIREMENT_QUEUE_CAPACITY;
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle, ThreadId};

pub(crate) const REAPER_THREAD_NAME: &str = "tasq-sparse-reaper";

#[derive(Debug)]
pub(crate) enum ReaperSpawnError {
    ZeroCapacity,
    ThreadSpawn(io::Error),
}

impl fmt::Display for ReaperSpawnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroCapacity => formatter.write_str("Reaper queue capacity must be non-zero"),
            Self::ThreadSpawn(error) => write!(formatter, "could not spawn Reaper thread: {error}"),
        }
    }
}

impl std::error::Error for ReaperSpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ZeroCapacity => None,
            Self::ThreadSpawn(error) => Some(error),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ReaperSendError<T> {
    Full(T),
    Disconnected(T),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReaperStats {
    /// Number of payloads explicitly received and dropped by the worker loop.
    pub(crate) retired_payloads: usize,
}

/// Bounded producer-side handle. It is intentionally not Clone: one owner
/// controls the lifetime of the queue, and dropping it closes the queue after
/// all accepted payloads have been drained by the worker.
pub(crate) struct ReaperQueue<T> {
    sender: Sender<T>,
    capacity: usize,
    gate: Arc<ReaperGate>,
}

/// Coordinates producer shutdown without taking a lock on the handoff path.
/// The worker closes the gate and waits for already-entered try_retire calls
/// to leave before draining the bounded queue.
struct ReaperGate {
    closed: AtomicBool,
    senders_in_flight: AtomicUsize,
}

/// Join handle for the dedicated resource-destruction thread.
///
/// Dropping this handle signals the worker to drain accepted payloads and exit;
/// it does not join or block the dropping thread. Use `join` or
/// `stop_and_join` from a control thread when completion must be observed.
pub(crate) struct ReaperWorker {
    join: Option<JoinHandle<ReaperStats>>,
    stop_sender: Sender<()>,
    thread_id: ThreadId,
}

impl<T: Send + 'static> ReaperQueue<T> {
    /// Starts the worker before returning the producer handle, so no payload
    /// can be queued before the receiver has an off-thread owner.
    pub(crate) fn spawn(capacity: usize) -> Result<(Self, ReaperWorker), ReaperSpawnError> {
        if capacity == 0 {
            return Err(ReaperSpawnError::ZeroCapacity);
        }

        let (retirement_sender, retirement_receiver) = bounded::<T>(capacity);
        let (stop_sender, stop_receiver) = bounded::<()>(1);
        let gate = Arc::new(ReaperGate {
            closed: AtomicBool::new(false),
            senders_in_flight: AtomicUsize::new(0),
        });
        let worker_gate = Arc::clone(&gate);

        let join = thread::Builder::new()
            .name(REAPER_THREAD_NAME.to_string())
            .spawn(move || run_reaper(retirement_receiver, stop_receiver, worker_gate))
            .map_err(ReaperSpawnError::ThreadSpawn)?;
        let thread_id = join.thread().id();

        Ok((
            Self {
                sender: retirement_sender,
                capacity,
                gate,
            },
            ReaperWorker {
                join: Some(join),
                stop_sender,
                thread_id,
            },
        ))
    }

    /// Uses the retirement capacity agreed in the Stage 4.1 runtime contract.
    pub(crate) fn spawn_default() -> Result<(Self, ReaperWorker), ReaperSpawnError> {
        Self::spawn(RETIREMENT_QUEUE_CAPACITY)
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    /// Non-blocking ownership transfer.
    ///
    /// On `Full` or `Disconnected`, the exact payload is returned to the
    /// caller. The caller must keep it in a pending-retire location and retry
    /// from a non-render control path; it must not drop it in the render loop.
    #[inline]
    pub(crate) fn try_retire(&self, payload: T) -> Result<(), ReaperSendError<T>> {
        if self.gate.closed.load(Ordering::Acquire) {
            return Err(ReaperSendError::Disconnected(payload));
        }

        self.gate.senders_in_flight.fetch_add(1, Ordering::AcqRel);
        if self.gate.closed.load(Ordering::Acquire) {
            self.gate.senders_in_flight.fetch_sub(1, Ordering::Release);
            return Err(ReaperSendError::Disconnected(payload));
        }

        let result = match self.sender.try_send(payload) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(payload)) => Err(ReaperSendError::Full(payload)),
            Err(TrySendError::Disconnected(payload)) => Err(ReaperSendError::Disconnected(payload)),
        };
        self.gate.senders_in_flight.fetch_sub(1, Ordering::Release);
        result
    }
}

impl ReaperWorker {
    pub(crate) fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    /// Waits for the queue to close, then joins the worker.
    ///
    /// The corresponding `ReaperQueue` must be dropped before calling this
    /// method. The worker drains accepted payloads before exiting.
    pub(crate) fn join(mut self) -> thread::Result<ReaperStats> {
        self.join
            .take()
            .expect("ReaperWorker join handle is present")
            .join()
    }

    /// Requests shutdown after producers have been quiesced. The worker drains
    /// all payloads already accepted by the queue before it exits. Any later
    /// `try_retire` call returns `Disconnected(payload)`.
    pub(crate) fn stop_and_join(mut self) -> thread::Result<ReaperStats> {
        let _ = self.stop_sender.try_send(());
        self.join
            .take()
            .expect("ReaperWorker join handle is present")
            .join()
    }
}

fn run_reaper<T>(
    retirement_receiver: Receiver<T>,
    stop_receiver: Receiver<()>,
    gate: Arc<ReaperGate>,
) -> ReaperStats {
    let mut retired_payloads = 0;

    loop {
        crossbeam_channel::select! {
            recv(retirement_receiver) -> message => match message {
                Ok(payload) => {
                    drop(payload);
                    retired_payloads += 1;
                }
                Err(_) => break,
            },
            recv(stop_receiver) -> _ => {
                // Close the producer gate first, then wait only on this worker
                // thread for any try_retire call that entered before shutdown.
                // No new payload can pass the second gate check after this.
                gate.closed.store(true, Ordering::Release);
                while gate.senders_in_flight.load(Ordering::Acquire) != 0 {
                    thread::yield_now();
                }

                // Drain everything accepted before dropping the receiver and
                // disconnecting the producer.
                while let Ok(payload) = retirement_receiver.try_recv() {
                    drop(payload);
                    retired_payloads += 1;
                }
                break;
            },
        }
    }

    ReaperStats { retired_payloads }
}

#[cfg(test)]
mod tests {
    use super::{ReaperQueue, ReaperSendError, ReaperSpawnError, REAPER_THREAD_NAME};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread::{self, ThreadId};
    use std::time::Duration;

    struct DropProbe {
        id: usize,
        dropped_on: Sender<(usize, ThreadId, String)>,
        block: Option<(Sender<()>, Receiver<()>)>,
    }

    impl DropProbe {
        fn new(id: usize, dropped_on: Sender<(usize, ThreadId, String)>) -> Self {
            Self {
                id,
                dropped_on,
                block: None,
            }
        }

        fn blocking(
            id: usize,
            dropped_on: Sender<(usize, ThreadId, String)>,
            started: Sender<()>,
            release: Receiver<()>,
        ) -> Self {
            Self {
                id,
                dropped_on,
                block: Some((started, release)),
            }
        }
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            if let Some((started, release)) = self.block.take() {
                let _ = started.send(());
                let _ = release.recv();
            }
            let thread = thread::current();
            let _ = self.dropped_on.send((
                self.id,
                thread.id(),
                thread.name().unwrap_or("unnamed").to_string(),
            ));
        }
    }

    fn drop_event_receiver() -> (
        Sender<(usize, ThreadId, String)>,
        Receiver<(usize, ThreadId, String)>,
    ) {
        mpsc::channel()
    }

    #[test]
    fn zero_capacity_is_rejected() {
        let result = ReaperQueue::<usize>::spawn(0);
        assert!(matches!(result, Err(ReaperSpawnError::ZeroCapacity)));
    }

    #[test]
    fn default_capacity_matches_the_runtime_retirement_contract() {
        let (queue, worker) = ReaperQueue::<usize>::spawn_default().unwrap();
        assert_eq!(
            queue.capacity(),
            super::super::sparse_runtime_contract::RETIREMENT_QUEUE_CAPACITY
        );
        drop(queue);
        let stats = worker.join().unwrap();
        assert_eq!(stats.retired_payloads, 0);
    }

    #[test]
    fn payload_destructors_run_on_the_named_reaper_thread() {
        let (drop_tx, drop_rx) = drop_event_receiver();
        let (queue, worker) = ReaperQueue::spawn(4).unwrap();
        let worker_id = worker.thread_id();

        assert!(queue.try_retire(DropProbe::new(1, drop_tx.clone())).is_ok());
        assert!(queue.try_retire(DropProbe::new(2, drop_tx.clone())).is_ok());
        drop(queue);

        let stats = worker.join().unwrap();
        assert_eq!(stats.retired_payloads, 2);

        for expected_id in [1, 2] {
            let (id, dropped_thread, thread_name) =
                drop_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(id, expected_id);
            assert_eq!(dropped_thread, worker_id);
            assert_eq!(thread_name, REAPER_THREAD_NAME);
        }
    }

    #[test]
    fn full_queue_returns_the_original_payload_without_dropping_it() {
        let (drop_tx, drop_rx) = drop_event_receiver();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (queue, worker) = ReaperQueue::spawn(1).unwrap();
        let worker_id = worker.thread_id();

        let first = DropProbe::blocking(1, drop_tx.clone(), started_tx, release_rx);
        assert!(queue.try_retire(first).is_ok());
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker should begin dropping the first payload");

        assert!(queue.try_retire(DropProbe::new(2, drop_tx.clone())).is_ok());
        let returned = match queue.try_retire(DropProbe::new(3, drop_tx.clone())) {
            Err(ReaperSendError::Full(payload)) => payload,
            Err(ReaperSendError::Disconnected(_)) => panic!("worker unexpectedly disconnected"),
            Ok(()) => panic!("third payload must be rejected by the full queue"),
        };
        assert_eq!(returned.id, 3);
        assert!(
            drop_rx.try_recv().is_err(),
            "no payload should have completed dropping yet"
        );

        release_tx.send(()).unwrap();
        drop(queue);
        let stats = worker.join().unwrap();
        assert_eq!(stats.retired_payloads, 2);

        for expected_id in [1, 2] {
            let (id, dropped_thread, thread_name) =
                drop_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(id, expected_id);
            assert_eq!(dropped_thread, worker_id);
            assert_eq!(thread_name, REAPER_THREAD_NAME);
        }

        let caller_thread = thread::current().id();
        drop(returned);
        let (id, dropped_thread, _) = drop_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(id, 3);
        assert_eq!(dropped_thread, caller_thread);
    }

    #[test]
    fn disconnected_queue_returns_the_original_payload_to_the_caller() {
        let (drop_tx, drop_rx) = drop_event_receiver();
        let (queue, worker) = ReaperQueue::spawn(1).unwrap();
        worker.stop_and_join().unwrap();

        let returned = match queue.try_retire(DropProbe::new(7, drop_tx)) {
            Err(ReaperSendError::Disconnected(payload)) => payload,
            Err(ReaperSendError::Full(_)) => panic!("queue cannot be full"),
            Ok(()) => panic!("stopped worker must disconnect the queue"),
        };
        assert_eq!(returned.id, 7);
        assert!(
            drop_rx.try_recv().is_err(),
            "failed handoff must preserve ownership"
        );

        let caller_thread = thread::current().id();
        drop(returned);
        let (id, dropped_thread, _) = drop_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(id, 7);
        assert_eq!(dropped_thread, caller_thread);
    }

    #[test]
    fn dropping_worker_handle_drains_accepted_payloads_off_thread() {
        let (drop_tx, drop_rx) = drop_event_receiver();
        let (queue, worker) = ReaperQueue::spawn(2).unwrap();
        let worker_id = worker.thread_id();

        assert!(queue
            .try_retire(DropProbe::new(20, drop_tx.clone()))
            .is_ok());
        drop(worker);

        let (id, dropped_thread, thread_name) =
            drop_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(id, 20);
        assert_eq!(dropped_thread, worker_id);
        assert_eq!(thread_name, REAPER_THREAD_NAME);

        drop(queue);
    }

    #[test]
    fn stop_drains_accepted_payloads_before_disconnect() {
        let (drop_tx, drop_rx) = drop_event_receiver();
        let (queue, worker) = ReaperQueue::spawn(4).unwrap();
        let worker_id = worker.thread_id();

        for id in 10..13 {
            assert!(queue
                .try_retire(DropProbe::new(id, drop_tx.clone()))
                .is_ok());
        }

        let stats = worker.stop_and_join().unwrap();
        assert_eq!(stats.retired_payloads, 3);
        let returned = match queue.try_retire(DropProbe::new(99, drop_tx.clone())) {
            Err(ReaperSendError::Disconnected(payload)) => payload,
            Err(ReaperSendError::Full(_)) => panic!("queue cannot be full"),
            Ok(()) => panic!("stopped worker must disconnect the queue"),
        };
        for expected_id in 10..13 {
            let (id, dropped_thread, thread_name) =
                drop_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(id, expected_id);
            assert_eq!(dropped_thread, worker_id);
            assert_eq!(thread_name, REAPER_THREAD_NAME);
        }

        let caller_thread = thread::current().id();
        drop(returned);
        let (id, dropped_thread, _) = drop_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(id, 99);
        assert_eq!(dropped_thread, caller_thread);
    }
}
