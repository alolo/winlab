//! Stage 4.1 test-only contract for the future dynamic Sparse Endpoint Mixer.
//!
//! This module is compiled only by `cargo test`. It does not participate in
//! production playback and deliberately carries source *tokens*, not live
//! decoder/player objects. Its purpose is to freeze slot identity, command
//! ordering, bounded-queue failure, and retirement semantics before Stage 4.2.

use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError, TrySendError};

/// Maximum number of simultaneously occupied audio slots on one endpoint.
/// A slot represents one Audio Cue, regardless of its output fan-out.
pub(crate) const MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT: usize = 256;

/// Control burst capacity. This is intentionally independent from slot count:
/// it can carry several full-endpoint command bursts without making the
/// render side wait. `try_send` reports saturation to the control side.
pub(crate) const CONTROL_QUEUE_CAPACITY: usize = 1_024;

/// One retirement burst can contain every occupied slot. If the reaper is
/// behind, the render side must retain the retiring slot and retry; it must
/// never block or destroy the source on the render thread.
pub(crate) const RETIREMENT_QUEUE_CAPACITY: usize = MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CueId(pub(crate) u128);

/// The generation is the existing per-Cue playback generation. Every
/// command that targets a running Cue must carry the same generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CueKey {
    pub(crate) id: CueId,
    pub(crate) generation: u64,
}

/// Test token standing in for a prepared source/Player payload. The real
/// source ownership transfer is deliberately not implemented in Stage 4.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PreparedSourceToken(pub(crate) u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LaunchTransactionId(pub(crate) u128);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlotToken {
    pub(crate) index: u16,
    pub(crate) generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoveReason {
    Stop,
    NaturalEnd,
    Reconfigure,
    Shutdown,
}

/// Control messages are intentionally small and Copy. The production Add
/// command will later carry a prebuilt, ownership-safe source handle; it must
/// not move a large source graph into or out of the callback by value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SparseMixerCommand {
    Add {
        key: CueKey,
        source: PreparedSourceToken,
    },
    Remove {
        key: CueKey,
        reason: RemoveReason,
    },
    UpdateRouting {
        key: CueKey,
        revision: u64,
    },
    UpdateGain {
        key: CueKey,
        revision: u64,
    },
    Arm {
        key: CueKey,
        transaction: LaunchTransactionId,
        start_frame: u64,
    },
    StopAll {
        request_id: u64,
    },
    Shutdown {
        request_id: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnqueueError {
    Full(SparseMixerCommand),
    ShutdownRequested(SparseMixerCommand),
    ReceiverDisconnected(SparseMixerCommand),
}

/// Single-producer handle owned by the endpoint's Worker control loop.
/// Keeping it non-Clone makes command order deterministic at this boundary.
pub(crate) struct SparseMixerHandle {
    sender: Sender<SparseMixerCommand>,
    shutdown_requested: bool,
}

pub(crate) struct SparseMixerReceiver {
    receiver: Receiver<SparseMixerCommand>,
}

impl SparseMixerHandle {
    pub(crate) fn bounded() -> (Self, SparseMixerReceiver) {
        let (sender, receiver) = bounded(CONTROL_QUEUE_CAPACITY);
        (
            Self {
                sender,
                shutdown_requested: false,
            },
            SparseMixerReceiver { receiver },
        )
    }

    /// Never waits for queue capacity. A full queue is an explicit control-
    /// side error; the audio/render side is never asked to block for a sender.
    pub(crate) fn try_send(&mut self, command: SparseMixerCommand) -> Result<(), EnqueueError> {
        if self.shutdown_requested {
            return Err(EnqueueError::ShutdownRequested(command));
        }

        let is_shutdown = matches!(command, SparseMixerCommand::Shutdown { .. });
        match self.sender.try_send(command) {
            Ok(()) => {
                if is_shutdown {
                    self.shutdown_requested = true;
                }
                Ok(())
            }
            Err(TrySendError::Full(command)) => Err(EnqueueError::Full(command)),
            Err(TrySendError::Disconnected(command)) => {
                Err(EnqueueError::ReceiverDisconnected(command))
            }
        }
    }
}

impl SparseMixerReceiver {
    pub(crate) fn try_recv(&self) -> Result<SparseMixerCommand, TryRecvError> {
        self.receiver.try_recv()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotLifecycle {
    Active,
    Retiring,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SlotState {
    key: CueKey,
    token: SlotToken,
    source: PreparedSourceToken,
    lifecycle: SlotLifecycle,
    routing_revision: u64,
    gain_revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotError {
    NoFreeSlot,
    DuplicateAdd,
    StaleGeneration { current: u64, requested: u64 },
    OlderGenerationStillActive { active: u64, requested: u64 },
    CueNotFound,
    AlreadyRetiring,
    CueRetiring,
    StaleRevision { current: u64, requested: u64 },
    InvalidSlotIndex,
    StaleSlotToken,
    SlotNotRetiring,
    SlotGenerationExhausted,
}

/// Fixed-capacity ownership/identity model used to test the Stage 4.1
/// contract. It performs no heap allocation in Add/Remove/update operations.
pub(crate) struct SparseEndpointSlotTable {
    slots: [Option<SlotState>; MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT],
    next_slot_generation: u64,
}

impl SparseEndpointSlotTable {
    pub(crate) fn new() -> Self {
        Self {
            slots: [None; MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT],
            next_slot_generation: 1,
        }
    }

    pub(crate) fn add(
        &mut self,
        key: CueKey,
        source: PreparedSourceToken,
    ) -> Result<SlotToken, SlotError> {
        for existing in self.slots.iter().flatten() {
            if existing.key.id != key.id {
                continue;
            }
            if existing.key.generation == key.generation {
                return Err(SlotError::DuplicateAdd);
            }
            if existing.key.generation > key.generation {
                return Err(SlotError::StaleGeneration {
                    current: existing.key.generation,
                    requested: key.generation,
                });
            }
            // A newer playback generation may be prepared while an older
            // generation is fading/retiring, but not while it is still active.
            if existing.lifecycle == SlotLifecycle::Active {
                return Err(SlotError::OlderGenerationStillActive {
                    active: existing.key.generation,
                    requested: key.generation,
                });
            }
        }

        let index = self
            .slots
            .iter()
            .position(Option::is_none)
            .ok_or(SlotError::NoFreeSlot)?;

        let next_generation = self
            .next_slot_generation
            .checked_add(1)
            .ok_or(SlotError::SlotGenerationExhausted)?;
        let token = SlotToken {
            index: index as u16,
            generation: self.next_slot_generation,
        };
        self.next_slot_generation = next_generation;

        self.slots[index] = Some(SlotState {
            key,
            token,
            source,
            lifecycle: SlotLifecycle::Active,
            routing_revision: 0,
            gain_revision: 0,
        });

        Ok(token)
    }

    /// Marks a slot as retiring. It remains occupied until `complete_retirement`
    /// acknowledges that ownership has safely moved to the reaper thread.
    pub(crate) fn begin_remove(&mut self, key: CueKey) -> Result<SlotToken, SlotError> {
        let index = self
            .slots
            .iter()
            .position(|slot| slot.is_some_and(|state| state.key == key));

        let Some(index) = index else {
            if let Some(existing_generation) = self
                .slots
                .iter()
                .flatten()
                .filter(|state| state.key.id == key.id)
                .map(|state| state.key.generation)
                .max()
            {
                if existing_generation > key.generation {
                    return Err(SlotError::StaleGeneration {
                        current: existing_generation,
                        requested: key.generation,
                    });
                }
            }
            return Err(SlotError::CueNotFound);
        };

        let state = self.slots[index].as_mut().expect("slot index was found");
        if state.lifecycle == SlotLifecycle::Retiring {
            return Err(SlotError::AlreadyRetiring);
        }
        state.lifecycle = SlotLifecycle::Retiring;
        Ok(state.token)
    }

    /// Frees a slot only after a matching reaper acknowledgement. The slot
    /// generation prevents an old acknowledgement from freeing a reused slot.
    pub(crate) fn complete_retirement(&mut self, token: SlotToken) -> Result<(), SlotError> {
        let index = token.index as usize;
        let Some(slot) = self.slots.get_mut(index) else {
            return Err(SlotError::InvalidSlotIndex);
        };
        let Some(state) = slot else {
            return Err(SlotError::StaleSlotToken);
        };
        if state.token != token {
            return Err(SlotError::StaleSlotToken);
        }
        if state.lifecycle != SlotLifecycle::Retiring {
            return Err(SlotError::SlotNotRetiring);
        }
        *slot = None;
        Ok(())
    }

    pub(crate) fn update_routing(&mut self, key: CueKey, revision: u64) -> Result<(), SlotError> {
        let state = self.find_exact_mut(key)?;
        if state.lifecycle == SlotLifecycle::Retiring {
            return Err(SlotError::CueRetiring);
        }
        if revision <= state.routing_revision {
            return Err(SlotError::StaleRevision {
                current: state.routing_revision,
                requested: revision,
            });
        }
        state.routing_revision = revision;
        Ok(())
    }

    pub(crate) fn update_gain(&mut self, key: CueKey, revision: u64) -> Result<(), SlotError> {
        let state = self.find_exact_mut(key)?;
        if state.lifecycle == SlotLifecycle::Retiring {
            return Err(SlotError::CueRetiring);
        }
        if revision <= state.gain_revision {
            return Err(SlotError::StaleRevision {
                current: state.gain_revision,
                requested: revision,
            });
        }
        state.gain_revision = revision;
        Ok(())
    }

    fn find_exact_mut(&mut self, key: CueKey) -> Result<&mut SlotState, SlotError> {
        if let Some(index) = self
            .slots
            .iter()
            .position(|slot| slot.is_some_and(|state| state.key == key))
        {
            return Ok(self.slots[index].as_mut().expect("slot index was found"));
        }

        if let Some(existing_generation) = self
            .slots
            .iter()
            .flatten()
            .filter(|state| state.key.id == key.id)
            .map(|state| state.key.generation)
            .max()
        {
            if existing_generation > key.generation {
                return Err(SlotError::StaleGeneration {
                    current: existing_generation,
                    requested: key.generation,
                });
            }
        }
        Err(SlotError::CueNotFound)
    }

    pub(crate) fn occupied_count(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    pub(crate) fn active_count(&self) -> usize {
        self.slots
            .iter()
            .flatten()
            .filter(|state| state.lifecycle == SlotLifecycle::Active)
            .count()
    }

    pub(crate) fn retiring_count(&self) -> usize {
        self.slots
            .iter()
            .flatten()
            .filter(|state| state.lifecycle == SlotLifecycle::Retiring)
            .count()
    }

    #[cfg(test)]
    fn source_token(&self, key: CueKey) -> Option<PreparedSourceToken> {
        self.slots
            .iter()
            .flatten()
            .find(|state| state.key == key)
            .map(|state| state.source)
    }
}

/// Test-only shape of the future endpoint mixer. In Stage 4.1 it owns only
/// the fixed slot contract; it does not pull PCM or implement `rodio::Source`.
pub(crate) struct SparseEndpointMixer {
    slots: SparseEndpointSlotTable,
}

impl SparseEndpointMixer {
    pub(crate) fn new() -> Self {
        Self {
            slots: SparseEndpointSlotTable::new(),
        }
    }

    pub(crate) fn add(
        &mut self,
        key: CueKey,
        source: PreparedSourceToken,
    ) -> Result<SlotToken, SlotError> {
        self.slots.add(key, source)
    }

    pub(crate) fn begin_remove(&mut self, key: CueKey) -> Result<SlotToken, SlotError> {
        self.slots.begin_remove(key)
    }

    pub(crate) fn complete_retirement(&mut self, token: SlotToken) -> Result<(), SlotError> {
        self.slots.complete_retirement(token)
    }

    pub(crate) fn update_routing(&mut self, key: CueKey, revision: u64) -> Result<(), SlotError> {
        self.slots.update_routing(key, revision)
    }

    pub(crate) fn update_gain(&mut self, key: CueKey, revision: u64) -> Result<(), SlotError> {
        self.slots.update_gain(key, revision)
    }

    pub(crate) fn active_count(&self) -> usize {
        self.slots.active_count()
    }

    pub(crate) fn occupied_count(&self) -> usize {
        self.slots.occupied_count()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CueId, CueKey, EnqueueError, LaunchTransactionId, PreparedSourceToken, RemoveReason,
        SlotError, SparseEndpointMixer, SparseEndpointSlotTable, SparseMixerCommand,
        SparseMixerHandle, CONTROL_QUEUE_CAPACITY, MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT,
        RETIREMENT_QUEUE_CAPACITY,
    };
    use crossbeam_channel::TryRecvError;

    fn key(id: u128, generation: u64) -> CueKey {
        CueKey {
            id: CueId(id),
            generation,
        }
    }

    fn add_command(id: u128, generation: u64) -> SparseMixerCommand {
        SparseMixerCommand::Add {
            key: key(id, generation),
            source: PreparedSourceToken(id as u64),
        }
    }

    #[test]
    fn runtime_capacities_are_explicit_and_independent() {
        assert_eq!(MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT, 256);
        assert_eq!(RETIREMENT_QUEUE_CAPACITY, 256);
        assert_eq!(CONTROL_QUEUE_CAPACITY, 1_024);
        assert_ne!(CONTROL_QUEUE_CAPACITY, MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT);
    }

    #[test]
    fn one_audio_cue_occupies_one_slot_independent_of_output_fanout() {
        let mut slots = SparseEndpointSlotTable::new();
        let token = slots
            .add(key(1, 1), PreparedSourceToken(100))
            .expect("one Cue should use one slot");

        // Routing destinations are deliberately not slot identities. A Cue
        // routed to 1, 16, or 32 destinations still has this one SlotToken.
        assert_eq!(token.index, 0);
        assert_eq!(slots.occupied_count(), 1);
        assert_eq!(slots.active_count(), 1);
        assert_eq!(
            slots.source_token(key(1, 1)),
            Some(PreparedSourceToken(100))
        );
    }

    #[test]
    fn endpoint_mixer_contract_owns_slot_lifecycle_without_rendering_pcm() {
        let mut mixer = SparseEndpointMixer::new();
        let cue = key(2, 1);
        let token = mixer
            .add(cue, PreparedSourceToken(200))
            .expect("contract mixer should accept one Cue");
        assert_eq!(mixer.active_count(), 1);
        assert_eq!(mixer.occupied_count(), 1);
        mixer
            .update_routing(cue, 1)
            .expect("routing revision should be accepted");
        mixer
            .update_gain(cue, 1)
            .expect("gain revision should be accepted");
        mixer
            .begin_remove(cue)
            .expect("Cue should enter retirement");
        assert_eq!(mixer.active_count(), 0);
        assert_eq!(mixer.occupied_count(), 1);
        mixer
            .complete_retirement(token)
            .expect("reaper acknowledgement should free the slot");
        assert_eq!(mixer.occupied_count(), 0);
    }

    #[test]
    fn slot_table_accepts_256_cues_and_rejects_the_257th() {
        let mut slots = SparseEndpointSlotTable::new();
        for id in 0..MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT as u128 {
            slots
                .add(key(id + 1, 1), PreparedSourceToken(id as u64))
                .expect("all 256 endpoint slots should be available");
        }

        assert_eq!(slots.active_count(), 256);
        assert_eq!(slots.occupied_count(), 256);
        assert_eq!(
            slots.add(key(10_000, 1), PreparedSourceToken(10_000)),
            Err(SlotError::NoFreeSlot)
        );
    }

    #[test]
    fn remove_or_update_of_an_unknown_cue_is_reported_without_side_effects() {
        let mut slots = SparseEndpointSlotTable::new();
        let unknown = key(6_000, 1);

        assert_eq!(slots.begin_remove(unknown), Err(SlotError::CueNotFound));
        assert_eq!(
            slots.update_routing(unknown, 1),
            Err(SlotError::CueNotFound)
        );
        assert_eq!(slots.update_gain(unknown, 1), Err(SlotError::CueNotFound));
        assert_eq!(slots.occupied_count(), 0);
    }

    #[test]
    fn duplicate_add_of_the_same_generation_is_rejected() {
        let mut slots = SparseEndpointSlotTable::new();
        slots
            .add(key(7, 4), PreparedSourceToken(70))
            .expect("first Add should succeed");

        assert_eq!(
            slots.add(key(7, 4), PreparedSourceToken(71)),
            Err(SlotError::DuplicateAdd)
        );
        assert_eq!(slots.occupied_count(), 1);
    }

    #[test]
    fn newer_generation_can_start_after_old_generation_enters_retirement() {
        let mut slots = SparseEndpointSlotTable::new();
        let old_key = key(8, 1);
        let new_key = key(8, 2);
        let old_token = slots
            .add(old_key, PreparedSourceToken(80))
            .expect("old generation should be added");

        assert_eq!(
            slots.add(new_key, PreparedSourceToken(81)),
            Err(SlotError::OlderGenerationStillActive {
                active: 1,
                requested: 2,
            })
        );

        slots
            .begin_remove(old_key)
            .expect("old generation should enter retirement");
        let new_token = slots
            .add(new_key, PreparedSourceToken(81))
            .expect("new generation may coexist with a retiring old generation");
        assert_ne!(old_token, new_token);
        assert_eq!(slots.active_count(), 1);
        assert_eq!(slots.retiring_count(), 1);
        assert_eq!(slots.occupied_count(), 2);
    }

    #[test]
    fn stale_generation_cannot_remove_or_update_newer_generation() {
        let mut slots = SparseEndpointSlotTable::new();
        let current = key(9, 3);
        slots
            .add(current, PreparedSourceToken(90))
            .expect("current generation should be added");

        assert_eq!(
            slots.begin_remove(key(9, 2)),
            Err(SlotError::StaleGeneration {
                current: 3,
                requested: 2,
            })
        );
        assert_eq!(
            slots.update_gain(key(9, 2), 1),
            Err(SlotError::StaleGeneration {
                current: 3,
                requested: 2,
            })
        );
        assert_eq!(slots.active_count(), 1);
    }

    #[test]
    fn retiring_slot_is_not_reused_until_reaper_acknowledges_it() {
        let mut slots = SparseEndpointSlotTable::new();
        let first_key = key(1, 1);
        let first_token = slots
            .add(first_key, PreparedSourceToken(1))
            .expect("first slot should be allocated");
        for id in 2..=MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT as u128 {
            slots
                .add(key(id, 1), PreparedSourceToken(id as u64))
                .expect("remaining slots should be allocated");
        }

        slots
            .begin_remove(first_key)
            .expect("first slot should enter retirement");
        assert_eq!(slots.active_count(), 255);
        assert_eq!(slots.retiring_count(), 1);
        assert_eq!(slots.occupied_count(), 256);
        assert_eq!(
            slots.add(key(20_000, 1), PreparedSourceToken(20_000)),
            Err(SlotError::NoFreeSlot)
        );

        slots
            .complete_retirement(first_token)
            .expect("reaper acknowledgement should release the slot");
        assert!(slots
            .add(key(20_000, 1), PreparedSourceToken(20_000))
            .is_ok());
    }

    #[test]
    fn stale_slot_token_cannot_release_a_reused_slot() {
        let mut slots = SparseEndpointSlotTable::new();
        let old_key = key(11, 1);
        let old_token = slots
            .add(old_key, PreparedSourceToken(110))
            .expect("old slot should be allocated");
        slots.begin_remove(old_key).expect("old slot should retire");
        slots
            .complete_retirement(old_token)
            .expect("old slot should be released");

        let new_token = slots
            .add(key(12, 1), PreparedSourceToken(120))
            .expect("a new Cue should reuse the vacant index");
        assert_eq!(new_token.index, old_token.index);
        assert_ne!(new_token.generation, old_token.generation);
        assert_eq!(
            slots.complete_retirement(old_token),
            Err(SlotError::StaleSlotToken)
        );
        assert_eq!(slots.active_count(), 1);
    }

    #[test]
    fn duplicate_remove_and_updates_during_retirement_are_rejected() {
        let mut slots = SparseEndpointSlotTable::new();
        let cue = key(13, 1);
        slots
            .add(cue, PreparedSourceToken(130))
            .expect("Cue should be added");
        slots
            .begin_remove(cue)
            .expect("first remove should succeed");

        assert_eq!(slots.begin_remove(cue), Err(SlotError::AlreadyRetiring));
        assert_eq!(slots.update_routing(cue, 1), Err(SlotError::CueRetiring));
        assert_eq!(slots.update_gain(cue, 1), Err(SlotError::CueRetiring));
    }

    #[test]
    fn routing_and_gain_updates_require_monotonic_revisions() {
        let mut slots = SparseEndpointSlotTable::new();
        let cue = key(14, 2);
        slots
            .add(cue, PreparedSourceToken(140))
            .expect("Cue should be added");

        assert_eq!(slots.update_routing(cue, 2), Ok(()));
        assert_eq!(
            slots.update_routing(cue, 1),
            Err(SlotError::StaleRevision {
                current: 2,
                requested: 1,
            })
        );
        assert_eq!(slots.update_gain(cue, 1), Ok(()));
        assert_eq!(
            slots.update_gain(cue, 1),
            Err(SlotError::StaleRevision {
                current: 1,
                requested: 1,
            })
        );
    }

    #[test]
    fn bounded_control_queue_preserves_command_order() {
        let (mut handle, receiver) = SparseMixerHandle::bounded();
        let commands = [
            add_command(21, 1),
            SparseMixerCommand::UpdateRouting {
                key: key(21, 1),
                revision: 1,
            },
            SparseMixerCommand::UpdateGain {
                key: key(21, 1),
                revision: 1,
            },
            SparseMixerCommand::Arm {
                key: key(21, 1),
                transaction: LaunchTransactionId(77),
                start_frame: 48_000,
            },
            SparseMixerCommand::Remove {
                key: key(21, 1),
                reason: RemoveReason::Stop,
            },
        ];

        for command in commands {
            handle
                .try_send(command)
                .expect("queue should have capacity");
        }
        for expected in commands {
            assert_eq!(receiver.try_recv(), Ok(expected));
        }
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn full_control_queue_returns_immediately_with_the_unsent_command() {
        let (mut handle, _receiver) = SparseMixerHandle::bounded();
        for request_id in 0..CONTROL_QUEUE_CAPACITY as u64 {
            handle
                .try_send(SparseMixerCommand::StopAll { request_id })
                .expect("queue should accept commands up to its fixed capacity");
        }

        let overflow = SparseMixerCommand::StopAll {
            request_id: CONTROL_QUEUE_CAPACITY as u64,
        };
        assert_eq!(handle.try_send(overflow), Err(EnqueueError::Full(overflow)));
    }

    #[test]
    fn shutdown_is_ordered_and_rejects_subsequent_commands() {
        let (mut handle, receiver) = SparseMixerHandle::bounded();
        let before_shutdown = add_command(31, 1);
        let shutdown = SparseMixerCommand::Shutdown { request_id: 900 };

        handle
            .try_send(before_shutdown)
            .expect("pre-shutdown command should be accepted");
        handle
            .try_send(shutdown)
            .expect("shutdown should be queued after prior commands");

        let after_shutdown = SparseMixerCommand::StopAll { request_id: 901 };
        assert_eq!(
            handle.try_send(after_shutdown),
            Err(EnqueueError::ShutdownRequested(after_shutdown))
        );
        assert_eq!(receiver.try_recv(), Ok(before_shutdown));
        assert_eq!(receiver.try_recv(), Ok(shutdown));
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn invalid_or_stale_retirement_acknowledgements_are_rejected() {
        let mut slots = SparseEndpointSlotTable::new();
        let active_key = key(40, 1);
        let active_token = slots
            .add(active_key, PreparedSourceToken(400))
            .expect("Cue should be added");

        assert_eq!(
            slots.complete_retirement(active_token),
            Err(SlotError::SlotNotRetiring)
        );
        assert_eq!(
            slots.complete_retirement(super::SlotToken {
                index: MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT as u16,
                generation: 1,
            }),
            Err(SlotError::InvalidSlotIndex)
        );
    }

    #[test]
    fn slot_generation_exhaustion_is_reported_without_allocating_a_slot() {
        let mut slots = SparseEndpointSlotTable::new();
        slots.next_slot_generation = u64::MAX;

        assert_eq!(
            slots.add(key(41, 1), PreparedSourceToken(410)),
            Err(SlotError::SlotGenerationExhausted)
        );
        assert_eq!(slots.occupied_count(), 0);
    }

    #[test]
    fn remove_reasons_are_preserved_in_the_control_protocol() {
        let (mut handle, receiver) = SparseMixerHandle::bounded();
        let reasons = [
            RemoveReason::Stop,
            RemoveReason::NaturalEnd,
            RemoveReason::Reconfigure,
            RemoveReason::Shutdown,
        ];
        for (index, reason) in reasons.into_iter().enumerate() {
            handle
                .try_send(SparseMixerCommand::Remove {
                    key: key(50 + index as u128, 1),
                    reason,
                })
                .expect("queue should accept remove reasons");
        }
        for (index, reason) in reasons.into_iter().enumerate() {
            assert_eq!(
                receiver.try_recv(),
                Ok(SparseMixerCommand::Remove {
                    key: key(50 + index as u128, 1),
                    reason,
                })
            );
        }
    }

    #[test]
    fn receiver_disconnect_is_reported_without_blocking() {
        let (mut handle, receiver) = SparseMixerHandle::bounded();
        drop(receiver);
        let command = SparseMixerCommand::StopAll { request_id: 1 };
        assert_eq!(
            handle.try_send(command),
            Err(EnqueueError::ReceiverDisconnected(command))
        );
    }
}
