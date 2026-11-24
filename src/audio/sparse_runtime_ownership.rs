//! Stage 4.2b2 test-only move-only Cue source ownership.
//!
//! This module models the ownership boundary for a future sparse endpoint
//! renderer. It stores prebuilt source/resource payloads in a fixed slot array,
//! transfers retired payloads to the Stage 4.2b1 Reaper, and frees a slot only
//! after the matching per-slot acknowledgement arrives.
//!
//! It is deliberately included only under `#[cfg(test)]`: no production
//! Rodio source, decoder, callback, or playback path is changed in this stage.

use super::sparse_runtime_contract::{
    CueKey, SlotError, SlotToken, MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT,
};
use super::sparse_runtime_reaper::{ReaperQueue, ReaperSendError, ReaperSpawnError, ReaperWorker};
use crossbeam_channel::{unbounded, Receiver, Sender, TryRecvError};
use std::fmt;

/// Move-only bundle prepared before a Cue is installed in a runtime slot.
///
/// `source` represents the already-built audio source chain; `route_resources`
/// represents resources owned by that Cue (for example, preallocated routing
/// scratch). Neither value is cloned or created by the ownership table.
pub(crate) struct CueSourcePayload<S, R> {
    source: S,
    route_resources: R,
}

impl<S, R> CueSourcePayload<S, R> {
    pub(crate) fn new(source: S, route_resources: R) -> Self {
        Self {
            source,
            route_resources,
        }
    }

    pub(crate) fn source(&self) -> &S {
        &self.source
    }

    pub(crate) fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }

    pub(crate) fn route_resources(&self) -> &R {
        &self.route_resources
    }

    pub(crate) fn route_resources_mut(&mut self) -> &mut R {
        &mut self.route_resources
    }

    /// Borrows the source and its preallocated render resources together.
    /// This lets the endpoint renderer pull PCM without moving the payload
    /// out of its ownership slot.
    pub(crate) fn parts_mut(&mut self) -> (&mut S, &mut R) {
        (&mut self.source, &mut self.route_resources)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnershipState {
    Active,
    RetirePending,
    AwaitingReaperAck,
}

struct OwnedCueSlot<P> {
    key: CueKey,
    token: SlotToken,
    state: OwnershipState,
    /// Present while the slot owns the payload or while a failed handoff is
    /// waiting to be retried. `None` means the Reaper owns it and an ack is due.
    payload: Option<P>,
}

/// An ownership-preserving Add error. The rejected payload remains available
/// to the caller and can be retried, retained, or explicitly sent to the Reaper.
#[must_use = "the rejected Cue payload must be retained or safely retired"]
pub(crate) struct AddSourceError<P> {
    pub(crate) error: SlotError,
    payload: P,
}

impl<P> AddSourceError<P> {
    pub(crate) fn payload(&self) -> &P {
        &self.payload
    }

    pub(crate) fn into_payload(self) -> P {
        self.payload
    }
}

impl<P> fmt::Debug for AddSourceError<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AddSourceError")
            .field("error", &self.error)
            .field("payload", &"<owned payload>")
            .finish()
    }
}

/// Result of trying to move a Retiring slot's payload to the bounded Reaper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetireStatus {
    Queued,
    PendingQueueFull,
    PendingDisconnected,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RetryReport {
    pub(crate) queued: usize,
    pub(crate) queue_full: usize,
    pub(crate) disconnected: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AckReport {
    pub(crate) received: usize,
    pub(crate) acknowledged: usize,
    pub(crate) stale_or_invalid: usize,
}

/// Summary of a control-side bulk retirement request.
/// Queue saturation never destroys a payload: it remains in its slot for retry.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RetireAllReport {
    pub(crate) attempted: usize,
    pub(crate) queued: usize,
    pub(crate) queue_full: usize,
    pub(crate) disconnected: usize,
}

/// A rejected Add can be safely handed to the Reaper without ever occupying a
/// slot. If the queue cannot accept it, the exact payload is returned.
pub(crate) enum RejectedAddRetireError<P> {
    QueueFull(P),
    Disconnected(P),
}

/// Wraps a payload so its destructor runs on the Reaper thread and its slot
/// acknowledgement is sent only after all payload fields have been dropped.
struct RetirementEnvelope<P> {
    token: Option<SlotToken>,
    payload: Option<P>,
    acknowledgement_sender: Sender<SlotToken>,
}

impl<P> RetirementEnvelope<P> {
    fn tracked(token: SlotToken, payload: P, acknowledgement_sender: Sender<SlotToken>) -> Self {
        Self {
            token: Some(token),
            payload: Some(payload),
            acknowledgement_sender,
        }
    }

    fn untracked(payload: P, acknowledgement_sender: Sender<SlotToken>) -> Self {
        Self {
            token: None,
            payload: Some(payload),
            acknowledgement_sender,
        }
    }

    /// Recovers a payload after a failed queue send without generating a false
    /// acknowledgement when the envelope itself is dropped on the caller.
    fn into_payload(mut self) -> P {
        self.token = None;
        self.payload
            .take()
            .expect("retirement envelope must still own its payload")
    }
}

impl<P> Drop for RetirementEnvelope<P> {
    fn drop(&mut self) {
        let Some(payload) = self.payload.take() else {
            return;
        };

        // Explicitly destroy the complete source/resource graph before making
        // the matching slot reusable through an acknowledgement.
        drop(payload);

        if let Some(token) = self.token {
            // The acknowledgement channel is naturally bounded by the fixed
            // slot count: a slot cannot be reused until its one ack is read.
            // Sending occurs on the Reaper thread, never on the render path.
            let _ = self.acknowledgement_sender.send(token);
        }
    }
}

/// Test-only owner for prebuilt Cue source/resource payloads.
///
/// The fixed array is the sole owner while a Cue is Active or while a Reaper
/// handoff is pending. Once the Reaper accepts a payload, the slot remains
/// occupied in `AwaitingReaperAck` until the exact `SlotToken` is acknowledged.
pub(crate) struct CueSourceOwnership<S, R>
where
    S: Send + 'static,
    R: Send + 'static,
{
    slots: [Option<OwnedCueSlot<CueSourcePayload<S, R>>>; MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT],
    next_slot_generation: u64,
    reaper_queue: ReaperQueue<RetirementEnvelope<CueSourcePayload<S, R>>>,
    acknowledgement_sender: Sender<SlotToken>,
    acknowledgement_receiver: Receiver<SlotToken>,
}

impl<S, R> CueSourceOwnership<S, R>
where
    S: Send + 'static,
    R: Send + 'static,
{
    /// Starts the Reaper before exposing the ownership table.
    pub(crate) fn spawn(reaper_capacity: usize) -> Result<(Self, ReaperWorker), ReaperSpawnError> {
        let (reaper_queue, worker) = ReaperQueue::spawn(reaper_capacity)?;
        // At most one acknowledgement can be outstanding per occupied slot.
        // The fixed 256-slot table therefore bounds the number of ack messages.
        let (acknowledgement_sender, acknowledgement_receiver) = unbounded();

        Ok((
            Self {
                slots: std::array::from_fn(|_| None),
                next_slot_generation: 1,
                reaper_queue,
                acknowledgement_sender,
                acknowledgement_receiver,
            },
            worker,
        ))
    }

    pub(crate) fn spawn_default() -> Result<(Self, ReaperWorker), ReaperSpawnError> {
        Self::spawn(super::sparse_runtime_contract::RETIREMENT_QUEUE_CAPACITY)
    }

    /// Installs a fully prepared, move-only payload in one fixed slot.
    /// Every rejection returns the original payload without dropping it.
    pub(crate) fn add(
        &mut self,
        key: CueKey,
        payload: CueSourcePayload<S, R>,
    ) -> Result<SlotToken, AddSourceError<CueSourcePayload<S, R>>> {
        if let Some(error) = self.validate_add(key) {
            return Err(AddSourceError { error, payload });
        }

        let Some(index) = self.slots.iter().position(Option::is_none) else {
            return Err(AddSourceError {
                error: SlotError::NoFreeSlot,
                payload,
            });
        };

        let Some(next_generation) = self.next_slot_generation.checked_add(1) else {
            return Err(AddSourceError {
                error: SlotError::SlotGenerationExhausted,
                payload,
            });
        };

        let token = SlotToken {
            index: index as u16,
            generation: self.next_slot_generation,
        };
        self.next_slot_generation = next_generation;
        self.slots[index] = Some(OwnedCueSlot {
            key,
            token,
            state: OwnershipState::Active,
            payload: Some(payload),
        });
        Ok(token)
    }

    fn validate_add(&self, key: CueKey) -> Option<SlotError> {
        let mut newest_generation = None;
        let mut active_older_generation = None;

        for slot in self.slots.iter().flatten() {
            if slot.key.id != key.id {
                continue;
            }
            if slot.key.generation == key.generation {
                return Some(SlotError::DuplicateAdd);
            }
            newest_generation = Some(
                newest_generation.map_or(slot.key.generation, |current: u64| {
                    current.max(slot.key.generation)
                }),
            );
            if slot.key.generation < key.generation && slot.state == OwnershipState::Active {
                active_older_generation = Some(
                    active_older_generation.map_or(slot.key.generation, |current: u64| {
                        current.max(slot.key.generation)
                    }),
                );
            }
        }

        if let Some(current) = newest_generation.filter(|current| *current > key.generation) {
            return Some(SlotError::StaleGeneration {
                current,
                requested: key.generation,
            });
        }
        active_older_generation.map(|active| SlotError::OlderGenerationStillActive {
            active,
            requested: key.generation,
        })
    }

    /// Marks a Cue Retiring and immediately attempts a non-blocking handoff.
    /// On queue saturation/disconnection, the payload remains in its slot.
    pub(crate) fn begin_retire(&mut self, key: CueKey) -> Result<RetireStatus, SlotError> {
        let index = self.find_slot_index(key)?;
        let slot = self.slots[index].as_mut().expect("slot index was found");
        if slot.state != OwnershipState::Active {
            return Err(SlotError::AlreadyRetiring);
        }
        slot.state = OwnershipState::RetirePending;
        Ok(self.try_handoff_at(index))
    }

    fn try_handoff_at(&mut self, index: usize) -> RetireStatus {
        let (token, payload) = {
            let slot = self.slots[index]
                .as_mut()
                .expect("pending retirement slot must exist");
            debug_assert_eq!(slot.state, OwnershipState::RetirePending);
            let payload = slot
                .payload
                .take()
                .expect("pending retirement slot must retain its payload");
            (slot.token, payload)
        };

        let envelope =
            RetirementEnvelope::tracked(token, payload, self.acknowledgement_sender.clone());
        match self.reaper_queue.try_retire(envelope) {
            Ok(()) => {
                self.slots[index]
                    .as_mut()
                    .expect("slot remains occupied until acknowledgement")
                    .state = OwnershipState::AwaitingReaperAck;
                RetireStatus::Queued
            }
            Err(ReaperSendError::Full(envelope)) => {
                let slot = self.slots[index]
                    .as_mut()
                    .expect("slot remains occupied after a full queue");
                slot.payload = Some(envelope.into_payload());
                RetireStatus::PendingQueueFull
            }
            Err(ReaperSendError::Disconnected(envelope)) => {
                let slot = self.slots[index]
                    .as_mut()
                    .expect("slot remains occupied after a disconnected queue");
                slot.payload = Some(envelope.into_payload());
                RetireStatus::PendingDisconnected
            }
        }
    }

    /// Marks every currently Active Cue as Retiring and attempts a bounded,
    /// non-blocking handoff for each payload. This is a control-side Stop All
    /// primitive; callers must retry queue-full slots and poll acknowledgements.
    ///
    /// Already-retiring slots are left untouched, so repeated calls cannot
    /// enqueue or destroy the same payload twice.
    pub(crate) fn begin_retire_all(&mut self) -> RetireAllReport {
        let mut report = RetireAllReport::default();

        for index in 0..self.slots.len() {
            let is_active = self.slots[index]
                .as_ref()
                .is_some_and(|slot| slot.state == OwnershipState::Active);
            if !is_active {
                continue;
            }

            self.slots[index]
                .as_mut()
                .expect("active slot must exist")
                .state = OwnershipState::RetirePending;
            report.attempted += 1;

            match self.try_handoff_at(index) {
                RetireStatus::Queued => report.queued += 1,
                RetireStatus::PendingQueueFull => report.queue_full += 1,
                RetireStatus::PendingDisconnected => report.disconnected += 1,
            }
        }

        report
    }

    /// Retries Retiring payloads whose earlier Reaper handoff was rejected.
    /// This method is intended for a non-render control/reaper-pump path.
    pub(crate) fn retry_pending_retirements(&mut self) -> RetryReport {
        let mut report = RetryReport::default();
        for index in 0..MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT {
            let pending = self.slots[index]
                .as_ref()
                .is_some_and(|slot| slot.state == OwnershipState::RetirePending);
            if !pending {
                continue;
            }
            match self.try_handoff_at(index) {
                RetireStatus::Queued => report.queued += 1,
                RetireStatus::PendingQueueFull => report.queue_full += 1,
                RetireStatus::PendingDisconnected => report.disconnected += 1,
            }
        }
        report
    }

    /// Consumes all currently available Reaper acknowledgements without
    /// waiting. Only an exact token for an AwaitingReaperAck slot frees it.
    pub(crate) fn poll_reaper_acknowledgements(&mut self) -> AckReport {
        let mut report = AckReport::default();
        loop {
            match self.acknowledgement_receiver.try_recv() {
                Ok(token) => {
                    report.received += 1;
                    if self.apply_reaper_acknowledgement(token).is_ok() {
                        report.acknowledged += 1;
                    } else {
                        report.stale_or_invalid += 1;
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        report
    }

    fn apply_reaper_acknowledgement(&mut self, token: SlotToken) -> Result<(), SlotError> {
        let index = token.index as usize;
        let Some(slot) = self.slots.get(index) else {
            return Err(SlotError::InvalidSlotIndex);
        };
        let Some(slot) = slot else {
            return Err(SlotError::StaleSlotToken);
        };
        if slot.token != token {
            return Err(SlotError::StaleSlotToken);
        }
        if slot.state != OwnershipState::AwaitingReaperAck || slot.payload.is_some() {
            return Err(SlotError::SlotNotRetiring);
        }
        self.slots[index] = None;
        Ok(())
    }

    /// Explicit control-thread recovery for a payload that could not be handed
    /// off because the Reaper is permanently disconnected. This does not run
    /// the destructor; ownership is returned to the caller. The slot is freed
    /// because the payload is no longer part of the runtime/render set.
    pub(crate) fn take_retained_payload_for_control(
        &mut self,
        token: SlotToken,
    ) -> Result<CueSourcePayload<S, R>, SlotError> {
        let index = token.index as usize;
        let Some(slot) = self.slots.get(index) else {
            return Err(SlotError::InvalidSlotIndex);
        };
        let Some(slot) = slot else {
            return Err(SlotError::StaleSlotToken);
        };
        if slot.token != token {
            return Err(SlotError::StaleSlotToken);
        }
        if slot.state != OwnershipState::RetirePending {
            return Err(SlotError::SlotNotRetiring);
        }

        let slot = self.slots[index]
            .as_mut()
            .expect("validated retained slot must exist");
        let payload = slot
            .payload
            .take()
            .expect("pending handoff must retain its payload");
        self.slots[index] = None;
        Ok(payload)
    }

    /// Safely transfers a rejected Add payload to the Reaper without a slot
    /// acknowledgement. On failure, the original payload is returned.
    pub(crate) fn try_retire_rejected_add(
        &self,
        rejected: AddSourceError<CueSourcePayload<S, R>>,
    ) -> Result<(), RejectedAddRetireError<CueSourcePayload<S, R>>> {
        self.try_retire_unattached_payload(rejected.payload)
    }

    /// Retries a previously rejected unattached payload without requiring a
    /// slot or generating an acknowledgement.
    pub(crate) fn try_retire_unattached_payload(
        &self,
        payload: CueSourcePayload<S, R>,
    ) -> Result<(), RejectedAddRetireError<CueSourcePayload<S, R>>> {
        let envelope = RetirementEnvelope::untracked(payload, self.acknowledgement_sender.clone());
        match self.reaper_queue.try_retire(envelope) {
            Ok(()) => Ok(()),
            Err(ReaperSendError::Full(envelope)) => {
                Err(RejectedAddRetireError::QueueFull(envelope.into_payload()))
            }
            Err(ReaperSendError::Disconnected(envelope)) => Err(
                RejectedAddRetireError::Disconnected(envelope.into_payload()),
            ),
        }
    }

    fn find_slot_index(&self, key: CueKey) -> Result<usize, SlotError> {
        if let Some(index) = self
            .slots
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|slot| slot.key == key))
        {
            return Ok(index);
        }

        if let Some(current_generation) = self
            .slots
            .iter()
            .flatten()
            .filter(|slot| slot.key.id == key.id)
            .map(|slot| slot.key.generation)
            .max()
        {
            if current_generation > key.generation {
                return Err(SlotError::StaleGeneration {
                    current: current_generation,
                    requested: key.generation,
                });
            }
        }
        Err(SlotError::CueNotFound)
    }

    /// Mutable access is restricted to Active slots. The future renderer can
    /// use this to pull source frames without ever taking ownership of the slot.
    pub(crate) fn active_payload_mut(
        &mut self,
        key: CueKey,
    ) -> Option<&mut CueSourcePayload<S, R>> {
        let index = self.find_slot_index(key).ok()?;
        let slot = self.slots[index].as_mut()?;
        if slot.state != OwnershipState::Active {
            return None;
        }
        slot.payload.as_mut()
    }

    /// Visits every Active payload in stable slot order without allocating.
    /// Retiring slots are deliberately excluded from the render set.
    pub(crate) fn for_each_active_payload_mut(
        &mut self,
        mut visit: impl FnMut(CueKey, SlotToken, &mut CueSourcePayload<S, R>),
    ) {
        for slot in self.slots.iter_mut().flatten() {
            if slot.state != OwnershipState::Active {
                continue;
            }
            if let Some(payload) = slot.payload.as_mut() {
                visit(slot.key, slot.token, payload);
            }
        }
    }

    pub(crate) fn slot_token(&self, key: CueKey) -> Option<SlotToken> {
        let index = self.find_slot_index(key).ok()?;
        self.slots[index].as_ref().map(|slot| slot.token)
    }

    pub(crate) fn active_count(&self) -> usize {
        self.slots
            .iter()
            .flatten()
            .filter(|slot| slot.state == OwnershipState::Active)
            .count()
    }

    pub(crate) fn occupied_count(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    pub(crate) fn retiring_count(&self) -> usize {
        self.slots
            .iter()
            .flatten()
            .filter(|slot| slot.state != OwnershipState::Active)
            .count()
    }

    pub(crate) fn pending_handoff_count(&self) -> usize {
        self.slots
            .iter()
            .flatten()
            .filter(|slot| slot.state == OwnershipState::RetirePending)
            .count()
    }

    pub(crate) fn awaiting_ack_count(&self) -> usize {
        self.slots
            .iter()
            .flatten()
            .filter(|slot| slot.state == OwnershipState::AwaitingReaperAck)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::super::sparse_runtime_contract::{CueId, CueKey, SlotError};
    use super::super::sparse_runtime_reaper::REAPER_THREAD_NAME;
    use super::{CueSourceOwnership, CueSourcePayload, RejectedAddRetireError, RetireStatus};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread::{self, ThreadId};
    use std::time::{Duration, Instant};

    fn key(id: u128, generation: u64) -> CueKey {
        CueKey {
            id: CueId(id),
            generation,
        }
    }

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
            let current = thread::current();
            let _ = self.dropped_on.send((
                self.id,
                current.id(),
                current.name().unwrap_or("unnamed").to_string(),
            ));
        }
    }

    fn payload(
        source_id: usize,
        resource_id: usize,
        drop_tx: &Sender<(usize, ThreadId, String)>,
    ) -> CueSourcePayload<DropProbe, DropProbe> {
        CueSourcePayload::new(
            DropProbe::new(source_id, drop_tx.clone()),
            DropProbe::new(resource_id, drop_tx.clone()),
        )
    }

    fn receive_drop(receiver: &Receiver<(usize, ThreadId, String)>) -> (usize, ThreadId, String) {
        receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("payload destructor should report its execution thread")
    }

    fn wait_until_empty<S, R>(owner: &mut CueSourceOwnership<S, R>)
    where
        S: Send + 'static,
        R: Send + 'static,
    {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let pending_before = owner.pending_handoff_count();
            let retry = owner.retry_pending_retirements();
            assert!(retry.queued <= pending_before);
            assert!(retry.queue_full + retry.disconnected <= pending_before);
            let acknowledgements = owner.poll_reaper_acknowledgements();
            assert_eq!(
                acknowledgements.acknowledged + acknowledgements.stale_or_invalid,
                acknowledgements.received
            );
            if owner.occupied_count() == 0 {
                return;
            }
            assert!(Instant::now() < deadline, "Cue ownership did not drain");
            thread::yield_now();
        }
    }

    #[test]
    fn active_payload_can_be_updated_but_retiring_payload_is_not_exposed() {
        let (mut owner, worker) = CueSourceOwnership::<usize, usize>::spawn_default().unwrap();
        let cue = key(100, 1);
        owner.add(cue, CueSourcePayload::new(1, 2)).unwrap();

        let payload = owner.active_payload_mut(cue).unwrap();
        *payload.source_mut() = 10;
        *payload.route_resources_mut() = 20;
        assert_eq!(payload.source(), &10);
        assert_eq!(payload.route_resources(), &20);

        assert_eq!(owner.begin_retire(cue), Ok(RetireStatus::Queued));
        assert!(owner.active_payload_mut(cue).is_none());
        assert_eq!(owner.retiring_count(), 1);
        wait_until_empty(&mut owner);
        drop(owner);
        assert_eq!(worker.join().unwrap().retired_payloads, 1);
    }

    #[test]
    fn source_and_route_resources_are_destroyed_on_reaper_before_slot_ack() {
        let (drop_tx, drop_rx) = mpsc::channel();
        let (mut owner, worker) = CueSourceOwnership::spawn(4).unwrap();
        let worker_id = worker.thread_id();
        let cue = key(1, 1);

        let token = owner.add(cue, payload(10, 11, &drop_tx)).unwrap();
        assert_eq!(owner.slot_token(cue), Some(token));
        assert_eq!(owner.active_count(), 1);
        assert_eq!(owner.begin_retire(cue), Ok(RetireStatus::Queued));
        assert_eq!(owner.occupied_count(), 1);
        assert_eq!(owner.awaiting_ack_count(), 1);

        for expected_id in [10, 11] {
            let (id, dropped_thread, thread_name) = receive_drop(&drop_rx);
            assert_eq!(id, expected_id);
            assert_eq!(dropped_thread, worker_id);
            assert_eq!(thread_name, REAPER_THREAD_NAME);
        }

        let acknowledgements = owner.poll_reaper_acknowledgements();
        assert_eq!(acknowledgements.received, 1);
        assert_eq!(acknowledgements.acknowledged, 1);
        assert_eq!(acknowledgements.stale_or_invalid, 0);
        assert_eq!(owner.occupied_count(), 0);

        drop(owner);
        assert_eq!(worker.join().unwrap().retired_payloads, 1);
    }

    #[test]
    fn retiring_slot_is_not_reused_before_ack_and_new_generation_can_start() {
        let (drop_tx, drop_rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (mut owner, worker) = CueSourceOwnership::spawn(4).unwrap();
        let worker_id = worker.thread_id();
        let old_key = key(2, 1);
        let old_payload = CueSourcePayload::new(
            DropProbe::blocking(20, drop_tx.clone(), started_tx, release_rx),
            DropProbe::new(21, drop_tx.clone()),
        );
        let old_token = owner.add(old_key, old_payload).unwrap();

        assert_eq!(owner.begin_retire(old_key), Ok(RetireStatus::Queued));
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("Reaper should enter the old source destructor");

        let new_key = key(2, 2);
        let new_token = owner.add(new_key, payload(22, 23, &drop_tx)).unwrap();
        assert_ne!(new_token.index, old_token.index);
        assert_ne!(new_token.generation, old_token.generation);
        assert_eq!(owner.occupied_count(), 2);
        assert_eq!(owner.awaiting_ack_count(), 1);

        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while owner.occupied_count() != 1 {
            owner.poll_reaper_acknowledgements();
            assert!(Instant::now() < deadline, "old slot was not acknowledged");
            thread::yield_now();
        }
        assert_eq!(owner.slot_token(new_key), Some(new_token));

        assert_eq!(owner.begin_retire(new_key), Ok(RetireStatus::Queued));
        wait_until_empty(&mut owner);
        for _ in 0..4 {
            let (_, dropped_thread, thread_name) = receive_drop(&drop_rx);
            assert_eq!(dropped_thread, worker_id);
            assert_eq!(thread_name, REAPER_THREAD_NAME);
        }
        drop(owner);
        assert_eq!(worker.join().unwrap().retired_payloads, 2);
    }

    #[test]
    fn full_reaper_queue_keeps_retiring_payload_in_slot_and_retry_succeeds() {
        let (drop_tx, drop_rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (mut owner, worker) = CueSourceOwnership::spawn(1).unwrap();
        let worker_id = worker.thread_id();

        let first = CueSourcePayload::new(
            DropProbe::blocking(30, drop_tx.clone(), started_tx, release_rx),
            DropProbe::new(31, drop_tx.clone()),
        );
        owner.add(key(3, 1), first).unwrap();
        owner.add(key(4, 1), payload(40, 41, &drop_tx)).unwrap();
        owner.add(key(5, 1), payload(50, 51, &drop_tx)).unwrap();

        assert_eq!(owner.begin_retire(key(3, 1)), Ok(RetireStatus::Queued));
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first payload should block the Reaper");
        assert_eq!(owner.begin_retire(key(4, 1)), Ok(RetireStatus::Queued));
        assert_eq!(
            owner.begin_retire(key(5, 1)),
            Ok(RetireStatus::PendingQueueFull)
        );
        assert_eq!(owner.pending_handoff_count(), 1);
        assert_eq!(owner.occupied_count(), 3);

        release_tx.send(()).unwrap();
        wait_until_empty(&mut owner);
        for _ in 0..6 {
            let (_, dropped_thread, thread_name) = receive_drop(&drop_rx);
            assert_eq!(dropped_thread, worker_id);
            assert_eq!(thread_name, REAPER_THREAD_NAME);
        }
        drop(owner);
        assert_eq!(worker.join().unwrap().retired_payloads, 3);
    }

    #[test]
    fn disconnected_reaper_keeps_payload_until_control_thread_recovers_it() {
        let (drop_tx, drop_rx) = mpsc::channel();
        let (mut owner, worker) = CueSourceOwnership::spawn(2).unwrap();
        let cue = key(6, 1);
        let token = owner.add(cue, payload(60, 61, &drop_tx)).unwrap();

        assert_eq!(worker.stop_and_join().unwrap().retired_payloads, 0);
        assert_eq!(
            owner.begin_retire(cue),
            Ok(RetireStatus::PendingDisconnected)
        );
        assert_eq!(owner.pending_handoff_count(), 1);
        assert_eq!(owner.occupied_count(), 1);
        assert!(
            drop_rx.try_recv().is_err(),
            "failed handoff must not drop payload"
        );

        let recovered = owner.take_retained_payload_for_control(token).unwrap();
        assert_eq!(recovered.source().id, 60);
        assert_eq!(recovered.route_resources().id, 61);
        assert_eq!(owner.occupied_count(), 0);
        drop(recovered);

        let caller_thread = thread::current().id();
        for expected_id in [60, 61] {
            let (id, dropped_thread, _) = receive_drop(&drop_rx);
            assert_eq!(id, expected_id);
            assert_eq!(dropped_thread, caller_thread);
        }
        drop(owner);
    }

    #[test]
    fn duplicate_add_returns_original_payload_and_can_retire_it_off_thread() {
        let (drop_tx, drop_rx) = mpsc::channel();
        let (mut owner, worker) = CueSourceOwnership::spawn(4).unwrap();
        let worker_id = worker.thread_id();
        let cue = key(7, 1);
        owner.add(cue, payload(70, 71, &drop_tx)).unwrap();

        let rejected = owner.add(cue, payload(72, 73, &drop_tx)).unwrap_err();
        assert_eq!(rejected.error, SlotError::DuplicateAdd);
        assert_eq!(rejected.payload().source().id, 72);
        assert!(owner.try_retire_rejected_add(rejected).is_ok());
        assert_eq!(owner.begin_retire(cue), Ok(RetireStatus::Queued));
        wait_until_empty(&mut owner);

        for _ in 0..4 {
            let (_, dropped_thread, thread_name) = receive_drop(&drop_rx);
            assert_eq!(dropped_thread, worker_id);
            assert_eq!(thread_name, REAPER_THREAD_NAME);
        }
        drop(owner);
        assert_eq!(worker.join().unwrap().retired_payloads, 2);
    }

    #[test]
    fn newer_generation_is_rejected_while_older_generation_is_active() {
        let (drop_tx, drop_rx) = mpsc::channel();
        let (mut owner, worker) = CueSourceOwnership::spawn(2).unwrap();
        let current = key(80, 1);
        owner.add(current, payload(800, 801, &drop_tx)).unwrap();

        let rejected = owner
            .add(key(80, 2), payload(802, 803, &drop_tx))
            .unwrap_err();
        assert_eq!(
            rejected.error,
            SlotError::OlderGenerationStillActive {
                active: 1,
                requested: 2,
            }
        );
        let returned = rejected.into_payload();
        assert_eq!(returned.source().id, 802);
        assert_eq!(owner.active_count(), 1);
        drop(returned);

        let caller_thread = thread::current().id();
        for expected_id in [802, 803] {
            let (id, dropped_thread, _) = receive_drop(&drop_rx);
            assert_eq!(id, expected_id);
            assert_eq!(dropped_thread, caller_thread);
        }

        assert_eq!(owner.begin_retire(current), Ok(RetireStatus::Queued));
        wait_until_empty(&mut owner);
        drop(owner);
        assert_eq!(worker.join().unwrap().retired_payloads, 1);
    }

    #[test]
    fn stale_generation_add_returns_payload_without_touching_current_source() {
        let (drop_tx, drop_rx) = mpsc::channel();
        let (mut owner, worker) = CueSourceOwnership::spawn(2).unwrap();
        let current = key(8, 3);
        owner.add(current, payload(80, 81, &drop_tx)).unwrap();

        let rejected = owner.add(key(8, 2), payload(82, 83, &drop_tx)).unwrap_err();
        assert_eq!(
            rejected.error,
            SlotError::StaleGeneration {
                current: 3,
                requested: 2,
            }
        );
        let returned = rejected.into_payload();
        assert_eq!(returned.source().id, 82);
        assert!(drop_rx.try_recv().is_err());
        drop(returned);

        let caller_thread = thread::current().id();
        for expected_id in [82, 83] {
            let (id, dropped_thread, _) = receive_drop(&drop_rx);
            assert_eq!(id, expected_id);
            assert_eq!(dropped_thread, caller_thread);
        }
        assert_eq!(owner.begin_retire(current), Ok(RetireStatus::Queued));
        wait_until_empty(&mut owner);
        drop(owner);
        assert_eq!(worker.join().unwrap().retired_payloads, 1);
    }

    #[test]
    fn stale_acknowledgement_cannot_free_a_slot_reused_by_another_cue() {
        let (drop_tx, drop_rx) = mpsc::channel();
        let (mut owner, worker) = CueSourceOwnership::spawn(4).unwrap();
        let old_key = key(9, 1);
        let old_token = owner.add(old_key, payload(90, 91, &drop_tx)).unwrap();
        assert_eq!(owner.begin_retire(old_key), Ok(RetireStatus::Queued));

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let report = owner.poll_reaper_acknowledgements();
            if report.acknowledged == 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Reaper acknowledgement did not arrive"
            );
            thread::yield_now();
        }
        assert_eq!(owner.occupied_count(), 0);

        let new_key = key(10, 1);
        let new_token = owner.add(new_key, payload(100, 101, &drop_tx)).unwrap();
        assert_eq!(new_token.index, old_token.index);
        assert_ne!(new_token.generation, old_token.generation);
        assert_eq!(
            owner.apply_reaper_acknowledgement(old_token),
            Err(SlotError::StaleSlotToken)
        );
        assert_eq!(owner.occupied_count(), 1);
        assert_eq!(owner.slot_token(new_key), Some(new_token));

        assert_eq!(owner.begin_retire(new_key), Ok(RetireStatus::Queued));
        wait_until_empty(&mut owner);
        for _ in 0..4 {
            let _ = receive_drop(&drop_rx);
        }
        drop(owner);
        assert_eq!(worker.join().unwrap().retired_payloads, 2);
    }

    #[test]
    fn full_slot_table_returns_rejected_payload_without_allocating_a_slot() {
        let (drop_tx, drop_rx) = mpsc::channel();
        let (mut owner, worker) = CueSourceOwnership::spawn(256).unwrap();
        let worker_id = worker.thread_id();
        for id in 1..=256_usize {
            owner
                .add(
                    key(id as u128, 1),
                    CueSourcePayload::new(DropProbe::new(id, drop_tx.clone()), ()),
                )
                .unwrap();
        }
        assert_eq!(owner.occupied_count(), 256);

        let rejected = owner
            .add(
                key(257, 1),
                CueSourcePayload::new(DropProbe::new(257, drop_tx.clone()), ()),
            )
            .unwrap_err();
        assert_eq!(rejected.error, SlotError::NoFreeSlot);
        assert_eq!(rejected.payload().source().id, 257);
        drop(rejected.into_payload());

        let caller_thread = thread::current().id();
        let (rejected_id, rejected_thread, _) = receive_drop(&drop_rx);
        assert_eq!(rejected_id, 257);
        assert_eq!(rejected_thread, caller_thread);

        for id in 1..=256_u128 {
            assert_eq!(owner.begin_retire(key(id, 1)), Ok(RetireStatus::Queued));
        }
        wait_until_empty(&mut owner);
        for expected_id in 1..=256_usize {
            let (id, dropped_thread, thread_name) = receive_drop(&drop_rx);
            assert_eq!(id, expected_id);
            assert_eq!(dropped_thread, worker_id);
            assert_eq!(thread_name, REAPER_THREAD_NAME);
        }
        drop(owner);
        assert_eq!(worker.join().unwrap().retired_payloads, 256);
    }

    #[test]
    fn repeated_remove_does_not_enqueue_or_destroy_a_payload_twice() {
        let (drop_tx, drop_rx) = mpsc::channel();
        let (mut owner, worker) = CueSourceOwnership::spawn(2).unwrap();
        let cue = key(11, 1);
        owner.add(cue, payload(110, 111, &drop_tx)).unwrap();

        assert_eq!(owner.begin_retire(cue), Ok(RetireStatus::Queued));
        assert_eq!(owner.begin_retire(cue), Err(SlotError::AlreadyRetiring));
        assert_eq!(owner.awaiting_ack_count(), 1);
        wait_until_empty(&mut owner);

        for expected_id in [110, 111] {
            let (id, _, _) = receive_drop(&drop_rx);
            assert_eq!(id, expected_id);
        }
        drop(owner);
        assert_eq!(worker.join().unwrap().retired_payloads, 1);
    }

    #[test]
    fn rejected_add_handoff_returns_payload_when_reaper_queue_is_full() {
        let (drop_tx, drop_rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (mut owner, worker) = CueSourceOwnership::spawn(1).unwrap();
        let worker_id = worker.thread_id();
        let existing = key(12, 1);
        let blocking = CueSourcePayload::new(
            DropProbe::blocking(120, drop_tx.clone(), started_tx, release_rx),
            DropProbe::new(121, drop_tx.clone()),
        );
        owner.add(existing, blocking).unwrap();
        assert_eq!(owner.begin_retire(existing), Ok(RetireStatus::Queued));
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("Reaper should be blocked in the first destructor");

        let filler_key = key(13, 1);
        owner.add(filler_key, payload(130, 131, &drop_tx)).unwrap();
        assert_eq!(owner.begin_retire(filler_key), Ok(RetireStatus::Queued));

        let rejected = owner
            .add(existing, payload(122, 123, &drop_tx))
            .unwrap_err();
        assert_eq!(rejected.error, SlotError::DuplicateAdd);
        let mut pending_payload = match owner.try_retire_rejected_add(rejected) {
            Err(RejectedAddRetireError::QueueFull(payload)) => payload,
            Err(RejectedAddRetireError::Disconnected(_)) => {
                panic!("Reaper unexpectedly disconnected")
            }
            Ok(()) => panic!("bounded Reaper queue should be full"),
        };
        assert_eq!(pending_payload.source().id, 122);
        assert!(drop_rx.try_recv().is_err());

        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match owner.try_retire_unattached_payload(pending_payload) {
                Ok(()) => break,
                Err(RejectedAddRetireError::QueueFull(payload)) => {
                    assert!(Instant::now() < deadline, "Reaper queue did not drain");
                    pending_payload = payload;
                    thread::yield_now();
                }
                Err(RejectedAddRetireError::Disconnected(_)) => {
                    panic!("Reaper unexpectedly disconnected during retry")
                }
            }
        }

        wait_until_empty(&mut owner);
        drop(owner);
        assert_eq!(worker.join().unwrap().retired_payloads, 3);
        for _ in 0..6 {
            let (_, dropped_thread, thread_name) = receive_drop(&drop_rx);
            assert_eq!(dropped_thread, worker_id);
            assert_eq!(thread_name, REAPER_THREAD_NAME);
        }
    }

    #[test]
    fn begin_retire_all_skips_existing_retirements_and_is_idempotent() {
        let (drop_tx, _drop_rx) = mpsc::channel();
        let (mut owner, worker) = CueSourceOwnership::spawn(4).unwrap();
        owner.add(key(20, 1), payload(200, 201, &drop_tx)).unwrap();
        owner.add(key(21, 1), payload(210, 211, &drop_tx)).unwrap();

        assert_eq!(owner.begin_retire(key(20, 1)), Ok(RetireStatus::Queued));

        let first = owner.begin_retire_all();
        assert_eq!(first.attempted, 1);
        assert_eq!(first.queued, 1);
        assert_eq!(first.queue_full, 0);
        assert_eq!(first.disconnected, 0);

        let second = owner.begin_retire_all();
        assert_eq!(second.attempted, 0);
        assert_eq!(second.queued, 0);
        assert_eq!(owner.occupied_count(), 2);

        wait_until_empty(&mut owner);
        drop(owner);
        assert_eq!(worker.join().unwrap().retired_payloads, 2);
    }

    #[test]
    fn bulk_retirement_saturation_preserves_all_256_payloads_until_retry() {
        let (drop_tx, drop_rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut release_rx = Some(release_rx);
        let (mut owner, worker) = CueSourceOwnership::spawn(1).unwrap();
        let worker_id = worker.thread_id();

        for id in 1..=256_usize {
            let source = if id == 1 {
                DropProbe::blocking(
                    id,
                    drop_tx.clone(),
                    started_tx.clone(),
                    release_rx.take().expect("blocking source is created once"),
                )
            } else {
                DropProbe::new(id, drop_tx.clone())
            };
            owner
                .add(key(id as u128, 1), CueSourcePayload::new(source, ()))
                .unwrap();
        }

        assert_eq!(owner.begin_retire(key(1, 1)), Ok(RetireStatus::Queued));
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first Reaper destructor should block");

        let report = owner.begin_retire_all();
        assert_eq!(report.attempted, 255);
        assert_eq!(report.queued, 1);
        assert_eq!(report.queue_full, 254);
        assert_eq!(report.disconnected, 0);
        assert_eq!(owner.pending_handoff_count(), 254);
        assert_eq!(owner.occupied_count(), 256);

        release_tx.send(()).unwrap();
        wait_until_empty(&mut owner);

        let mut seen = vec![false; 257];
        for _ in 0..256 {
            let (id, dropped_thread, thread_name) = receive_drop(&drop_rx);
            assert!((1..=256).contains(&id));
            assert!(!seen[id], "payload {id} was destroyed more than once");
            seen[id] = true;
            assert_eq!(dropped_thread, worker_id);
            assert_eq!(thread_name, REAPER_THREAD_NAME);
        }
        assert!(seen[1..].iter().all(|was_seen| *was_seen));

        drop(owner);
        assert_eq!(worker.join().unwrap().retired_payloads, 256);
    }

    #[test]
    fn shutdown_after_bulk_retirement_drains_all_active_payloads_on_reaper() {
        let (drop_tx, drop_rx) = mpsc::channel();
        let (mut owner, worker) = CueSourceOwnership::spawn(256).unwrap();
        let worker_id = worker.thread_id();

        for id in 1..=256_usize {
            owner
                .add(
                    key(id as u128, 1),
                    CueSourcePayload::new(DropProbe::new(id, drop_tx.clone()), ()),
                )
                .unwrap();
        }

        let report = owner.begin_retire_all();
        assert_eq!(report.attempted, 256);
        assert_eq!(report.queued, 256);
        assert_eq!(report.queue_full, 0);
        assert_eq!(report.disconnected, 0);
        assert_eq!(owner.active_count(), 0);
        assert_eq!(owner.occupied_count(), 256);

        // Dropping the producer closes the queue. The Reaper drains every
        // accepted envelope before its JoinHandle completes.
        drop(owner);
        assert_eq!(worker.join().unwrap().retired_payloads, 256);

        let mut seen = vec![false; 257];
        for _ in 0..256 {
            let (id, dropped_thread, thread_name) = receive_drop(&drop_rx);
            assert!((1..=256).contains(&id));
            assert!(!seen[id], "payload {id} was destroyed more than once");
            seen[id] = true;
            assert_eq!(dropped_thread, worker_id);
            assert_eq!(thread_name, REAPER_THREAD_NAME);
        }
        assert!(seen[1..].iter().all(|was_seen| *was_seen));
    }
}
