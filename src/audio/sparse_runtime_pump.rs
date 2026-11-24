//! Stage 4.2a test-only command pump for the Sparse Endpoint Mixer.
//!
//! This stage connects the Stage 4.1 bounded command protocol to the fixed
//! slot lifecycle model. It deliberately does not render PCM, own decoder
//! objects, or participate in production playback. Those concerns remain
//! isolated for Stage 4.2b.

use super::sparse_runtime_contract::{
    CueKey, LaunchTransactionId, PreparedSourceToken, SlotError, SlotToken, SparseEndpointMixer,
    SparseMixerCommand, SparseMixerReceiver, MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT,
};
use crossbeam_channel::TryRecvError;

/// Maximum control messages consumed at one endpoint-frame boundary.
/// A full 256-Cue install can span several frames; launch scheduling must
/// account for that until an atomic batch command is implemented.
pub(crate) const MAX_COMMANDS_PER_FRAME: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArmedCueState {
    pub(crate) transaction: LaunchTransactionId,
    pub(crate) start_frame: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CommandProcessReport {
    pub(crate) processed: usize,
    pub(crate) applied: usize,
    pub(crate) rejected: usize,
}

/// Test-only consumer that applies bounded control messages to the Stage 4.1
/// slot table. The arrays mirror slot identity and retirement state so the
/// command pump never needs to allocate while processing a frame boundary.
pub(crate) struct SparseRuntimePump {
    mixer: SparseEndpointMixer,
    receiver: SparseMixerReceiver,
    slot_keys: [Option<CueKey>; MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT],
    slot_tokens: [Option<SlotToken>; MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT],
    retiring: [bool; MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT],
    armed: [Option<ArmedCueState>; MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT],
    shutdown_requested: bool,
    last_error: Option<SlotError>,
}

impl SparseRuntimePump {
    pub(crate) fn new(receiver: SparseMixerReceiver) -> Self {
        Self {
            mixer: SparseEndpointMixer::new(),
            receiver,
            slot_keys: [None; MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT],
            slot_tokens: [None; MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT],
            retiring: [false; MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT],
            armed: [None; MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT],
            shutdown_requested: false,
            last_error: None,
        }
    }

    /// Called by the future render loop at an endpoint-frame boundary.
    /// `try_recv` is used exclusively: this function never waits for a
    /// producer and never allocates.
    pub(crate) fn process_pending_commands(&mut self) -> CommandProcessReport {
        let mut report = CommandProcessReport::default();

        while report.processed < MAX_COMMANDS_PER_FRAME {
            let command = match self.receiver.try_recv() {
                Ok(command) => command,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };

            report.processed += 1;
            if self.apply_command(command) {
                report.applied += 1;
            } else {
                report.rejected += 1;
            }
        }

        report
    }

    fn apply_command(&mut self, command: SparseMixerCommand) -> bool {
        match command {
            SparseMixerCommand::Add { key, source } => self.apply_add(key, source),
            SparseMixerCommand::Remove { key, reason: _ } => self.apply_remove(key),
            SparseMixerCommand::UpdateRouting { key, revision } => {
                match self.mixer.update_routing(key, revision) {
                    Ok(()) => true,
                    Err(error) => self.reject(error),
                }
            }
            SparseMixerCommand::UpdateGain { key, revision } => {
                match self.mixer.update_gain(key, revision) {
                    Ok(()) => true,
                    Err(error) => self.reject(error),
                }
            }
            SparseMixerCommand::Arm {
                key,
                transaction,
                start_frame,
            } => self.apply_arm(key, transaction, start_frame),
            SparseMixerCommand::StopAll { request_id: _ } => {
                self.begin_stop_all();
                true
            }
            SparseMixerCommand::Shutdown { request_id: _ } => {
                self.shutdown_requested = true;
                self.begin_stop_all();
                true
            }
        }
    }

    fn apply_add(&mut self, key: CueKey, source: PreparedSourceToken) -> bool {
        match self.mixer.add(key, source) {
            Ok(token) => {
                let index = token.index as usize;
                self.slot_keys[index] = Some(key);
                self.slot_tokens[index] = Some(token);
                self.retiring[index] = false;
                self.armed[index] = None;
                true
            }
            Err(error) => self.reject(error),
        }
    }

    fn apply_remove(&mut self, key: CueKey) -> bool {
        match self.mixer.begin_remove(key) {
            Ok(token) => {
                let index = token.index as usize;
                self.retiring[index] = true;
                self.armed[index] = None;
                true
            }
            Err(error) => self.reject(error),
        }
    }

    fn apply_arm(
        &mut self,
        key: CueKey,
        transaction: LaunchTransactionId,
        start_frame: u64,
    ) -> bool {
        match self.find_slot(key) {
            Ok(index) if !self.retiring[index] => {
                self.armed[index] = Some(ArmedCueState {
                    transaction,
                    start_frame,
                });
                true
            }
            Ok(_) => self.reject(SlotError::CueRetiring),
            Err(error) => self.reject(error),
        }
    }

    fn begin_stop_all(&mut self) {
        for index in 0..MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT {
            if self.slot_keys[index].is_none() || self.retiring[index] {
                continue;
            }

            let key = self.slot_keys[index].expect("slot key was checked");
            match self.mixer.begin_remove(key) {
                Ok(_) => {
                    self.retiring[index] = true;
                    self.armed[index] = None;
                }
                Err(error) => {
                    self.last_error = Some(error);
                }
            }
        }
    }

    fn find_slot(&self, key: CueKey) -> Result<usize, SlotError> {
        if let Some(index) = self
            .slot_keys
            .iter()
            .position(|candidate| *candidate == Some(key))
        {
            return Ok(index);
        }

        if let Some(current_generation) = self
            .slot_keys
            .iter()
            .flatten()
            .filter(|candidate| candidate.id == key.id)
            .map(|candidate| candidate.generation)
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

    fn reject(&mut self, error: SlotError) -> bool {
        self.last_error = Some(error);
        false
    }

    /// Simulates the acknowledgement delivered by the future reaper. A slot
    /// remains occupied until this exact token is acknowledged.
    pub(crate) fn acknowledge_retirement(&mut self, token: SlotToken) -> Result<(), SlotError> {
        self.mixer.complete_retirement(token)?;
        let index = token.index as usize;
        if self.slot_tokens[index] == Some(token) {
            self.slot_keys[index] = None;
            self.slot_tokens[index] = None;
            self.retiring[index] = false;
            self.armed[index] = None;
        }
        Ok(())
    }

    /// Copies pending retirement tokens into caller-owned storage without
    /// allocating. Returns the number written.
    pub(crate) fn copy_pending_retirements(&self, output: &mut [SlotToken]) -> usize {
        let mut written = 0;
        for index in 0..MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT {
            if !self.retiring[index] {
                continue;
            }
            let Some(token) = self.slot_tokens[index] else {
                continue;
            };
            if written == output.len() {
                break;
            }
            output[written] = token;
            written += 1;
        }
        written
    }

    pub(crate) fn armed_state(&self, key: CueKey) -> Option<ArmedCueState> {
        let index = self.find_slot(key).ok()?;
        if self.retiring[index] {
            return None;
        }
        self.armed[index]
    }

    pub(crate) fn is_retiring(&self, key: CueKey) -> bool {
        self.find_slot(key)
            .map(|index| self.retiring[index])
            .unwrap_or(false)
    }

    pub(crate) fn active_count(&self) -> usize {
        self.mixer.active_count()
    }

    pub(crate) fn occupied_count(&self) -> usize {
        self.mixer.occupied_count()
    }

    pub(crate) fn retiring_count(&self) -> usize {
        self.retiring.iter().filter(|retiring| **retiring).count()
    }

    pub(crate) fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    pub(crate) fn take_last_error(&mut self) -> Option<SlotError> {
        self.last_error.take()
    }
}

#[cfg(test)]
mod tests {
    use super::super::sparse_runtime_contract::{
        CueId, CueKey, EnqueueError, LaunchTransactionId, PreparedSourceToken, RemoveReason,
        SlotError, SlotToken, SparseMixerCommand, SparseMixerHandle,
        MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT,
    };
    use super::{ArmedCueState, SparseRuntimePump, MAX_COMMANDS_PER_FRAME};

    fn key(id: u128, generation: u64) -> CueKey {
        CueKey {
            id: CueId(id),
            generation,
        }
    }

    fn add(id: u128, generation: u64) -> SparseMixerCommand {
        SparseMixerCommand::Add {
            key: key(id, generation),
            source: PreparedSourceToken(id as u64),
        }
    }

    fn runtime() -> (SparseMixerHandle, SparseRuntimePump) {
        let (handle, receiver) = SparseMixerHandle::bounded();
        (handle, SparseRuntimePump::new(receiver))
    }

    #[test]
    fn queued_add_is_applied_only_when_the_frame_boundary_pump_runs() {
        let (mut handle, mut pump) = runtime();
        handle.try_send(add(1, 1)).unwrap();

        assert_eq!(pump.active_count(), 0);
        let report = pump.process_pending_commands();

        assert_eq!(report.processed, 1);
        assert_eq!(report.applied, 1);
        assert_eq!(report.rejected, 0);
        assert_eq!(pump.active_count(), 1);
        assert_eq!(pump.occupied_count(), 1);
    }

    #[test]
    fn add_arm_and_remove_are_applied_in_fifo_order() {
        let (mut handle, mut pump) = runtime();
        let cue = key(2, 4);
        handle.try_send(add(2, 4)).unwrap();
        handle
            .try_send(SparseMixerCommand::Arm {
                key: cue,
                transaction: LaunchTransactionId(77),
                start_frame: 48_000,
            })
            .unwrap();
        handle
            .try_send(SparseMixerCommand::Remove {
                key: cue,
                reason: RemoveReason::Stop,
            })
            .unwrap();

        let report = pump.process_pending_commands();
        assert_eq!(report.processed, 3);
        assert_eq!(report.applied, 3);
        assert_eq!(pump.active_count(), 0);
        assert_eq!(pump.occupied_count(), 1);
        assert!(pump.armed_state(cue).is_none());
        assert!(pump.is_retiring(cue));
    }

    #[test]
    fn arm_preserves_launch_transaction_and_absolute_start_frame() {
        let (mut handle, mut pump) = runtime();
        let cue = key(3, 1);
        let transaction = LaunchTransactionId(1234);
        handle.try_send(add(3, 1)).unwrap();
        handle
            .try_send(SparseMixerCommand::Arm {
                key: cue,
                transaction,
                start_frame: 96_000,
            })
            .unwrap();

        let report = pump.process_pending_commands();
        assert_eq!(report.applied, 2);
        assert_eq!(
            pump.armed_state(cue),
            Some(ArmedCueState {
                transaction,
                start_frame: 96_000,
            })
        );
    }

    #[test]
    fn duplicate_add_is_rejected_without_overwriting_the_original_slot() {
        let (mut handle, mut pump) = runtime();
        handle.try_send(add(4, 1)).unwrap();
        handle.try_send(add(4, 1)).unwrap();

        let report = pump.process_pending_commands();
        assert_eq!(report.processed, 2);
        assert_eq!(report.applied, 1);
        assert_eq!(report.rejected, 1);
        assert_eq!(pump.active_count(), 1);
        assert_eq!(pump.take_last_error(), Some(SlotError::DuplicateAdd));
    }

    #[test]
    fn stale_generation_commands_cannot_remove_or_arm_a_newer_cue() {
        let (mut handle, mut pump) = runtime();
        handle.try_send(add(5, 3)).unwrap();
        handle
            .try_send(SparseMixerCommand::Remove {
                key: key(5, 2),
                reason: RemoveReason::Stop,
            })
            .unwrap();
        handle
            .try_send(SparseMixerCommand::Arm {
                key: key(5, 2),
                transaction: LaunchTransactionId(9),
                start_frame: 10,
            })
            .unwrap();

        let report = pump.process_pending_commands();
        assert_eq!(report.applied, 1);
        assert_eq!(report.rejected, 2);
        assert_eq!(pump.active_count(), 1);
        assert_eq!(
            pump.take_last_error(),
            Some(SlotError::StaleGeneration {
                current: 3,
                requested: 2,
            })
        );
    }

    #[test]
    fn routing_and_gain_commands_are_consumed_in_revision_order() {
        let (mut handle, mut pump) = runtime();
        let cue = key(6, 1);
        handle.try_send(add(6, 1)).unwrap();
        handle
            .try_send(SparseMixerCommand::UpdateRouting {
                key: cue,
                revision: 1,
            })
            .unwrap();
        handle
            .try_send(SparseMixerCommand::UpdateGain {
                key: cue,
                revision: 1,
            })
            .unwrap();
        handle
            .try_send(SparseMixerCommand::UpdateRouting {
                key: cue,
                revision: 1,
            })
            .unwrap();

        let report = pump.process_pending_commands();
        assert_eq!(report.processed, 4);
        assert_eq!(report.applied, 3);
        assert_eq!(report.rejected, 1);
        assert_eq!(
            pump.take_last_error(),
            Some(SlotError::StaleRevision {
                current: 1,
                requested: 1,
            })
        );
        assert_eq!(pump.active_count(), 1);
    }

    #[test]
    fn remove_keeps_slot_occupied_until_matching_retirement_acknowledgement() {
        let (mut handle, mut pump) = runtime();
        let cue = key(7, 1);
        handle.try_send(add(7, 1)).unwrap();
        handle
            .try_send(SparseMixerCommand::Remove {
                key: cue,
                reason: RemoveReason::Stop,
            })
            .unwrap();
        pump.process_pending_commands();

        assert_eq!(pump.active_count(), 0);
        assert_eq!(pump.occupied_count(), 1);
        assert_eq!(pump.retiring_count(), 1);

        let mut tokens = [SlotToken {
            index: 0,
            generation: 0,
        }; MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT];
        let count = pump.copy_pending_retirements(&mut tokens);
        assert_eq!(count, 1);
        pump.acknowledge_retirement(tokens[0]).unwrap();

        assert_eq!(pump.occupied_count(), 0);
        assert_eq!(pump.retiring_count(), 0);
    }

    #[test]
    fn stale_retirement_acknowledgement_does_not_clear_a_reused_slot() {
        let (mut handle, mut pump) = runtime();
        let first = key(8, 1);
        handle.try_send(add(8, 1)).unwrap();
        handle
            .try_send(SparseMixerCommand::Remove {
                key: first,
                reason: RemoveReason::Stop,
            })
            .unwrap();
        pump.process_pending_commands();

        let mut tokens = [SlotToken {
            index: 0,
            generation: 0,
        }; MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT];
        assert_eq!(pump.copy_pending_retirements(&mut tokens), 1);
        let old_token = tokens[0];
        pump.acknowledge_retirement(old_token).unwrap();

        handle.try_send(add(9, 1)).unwrap();
        pump.process_pending_commands();
        assert_eq!(pump.active_count(), 1);

        assert_eq!(
            pump.acknowledge_retirement(old_token),
            Err(SlotError::StaleSlotToken)
        );
        assert_eq!(pump.active_count(), 1);
        assert_eq!(pump.occupied_count(), 1);
    }

    #[test]
    fn stop_all_retires_every_active_slot_and_waits_for_acknowledgements() {
        let (mut handle, mut pump) = runtime();
        for id in 10..13 {
            handle.try_send(add(id, 1)).unwrap();
        }
        handle
            .try_send(SparseMixerCommand::StopAll { request_id: 100 })
            .unwrap();

        let report = pump.process_pending_commands();
        assert_eq!(report.applied, 4);
        assert_eq!(pump.active_count(), 0);
        assert_eq!(pump.occupied_count(), 3);
        assert_eq!(pump.retiring_count(), 3);

        let mut tokens = [SlotToken {
            index: 0,
            generation: 0,
        }; MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT];
        let count = pump.copy_pending_retirements(&mut tokens);
        assert_eq!(count, 3);
        for token in tokens.into_iter().take(count) {
            pump.acknowledge_retirement(token).unwrap();
        }

        assert_eq!(pump.occupied_count(), 0);
    }

    #[test]
    fn shutdown_retires_active_slots_and_marks_runtime_closed() {
        let (mut handle, mut pump) = runtime();
        let cue = key(13, 1);
        handle.try_send(add(13, 1)).unwrap();
        handle
            .try_send(SparseMixerCommand::Shutdown { request_id: 200 })
            .unwrap();
        let after_shutdown = SparseMixerCommand::StopAll { request_id: 201 };
        assert_eq!(
            handle.try_send(after_shutdown),
            Err(EnqueueError::ShutdownRequested(after_shutdown))
        );

        let report = pump.process_pending_commands();
        assert_eq!(report.applied, 2);
        assert!(pump.shutdown_requested());
        assert_eq!(pump.active_count(), 0);
        assert_eq!(pump.occupied_count(), 1);
        assert!(pump.is_retiring(cue));
    }

    #[test]
    fn command_processing_is_bounded_per_frame_and_preserves_fifo_order() {
        let (mut handle, mut pump) = runtime();
        let boundary_key = key(30_000, 1);

        for id in 0..MAX_COMMANDS_PER_FRAME - 1 {
            handle.try_send(add(20_000 + id as u128, 1)).unwrap();
        }
        handle.try_send(add(30_000, 1)).unwrap();
        handle
            .try_send(SparseMixerCommand::Remove {
                key: boundary_key,
                reason: RemoveReason::Stop,
            })
            .unwrap();
        handle.try_send(add(30_001, 1)).unwrap();

        let first = pump.process_pending_commands();
        assert_eq!(first.processed, MAX_COMMANDS_PER_FRAME);
        assert_eq!(first.applied, MAX_COMMANDS_PER_FRAME);
        assert_eq!(pump.active_count(), MAX_COMMANDS_PER_FRAME);

        let second = pump.process_pending_commands();
        assert_eq!(second.processed, 2);
        assert_eq!(second.applied, 2);
        assert_eq!(second.rejected, 0);
        assert_eq!(pump.active_count(), MAX_COMMANDS_PER_FRAME);
        assert_eq!(pump.occupied_count(), MAX_COMMANDS_PER_FRAME + 1);
        assert!(pump.is_retiring(boundary_key));
    }

    #[test]
    fn full_endpoint_accepts_256_add_commands_and_rejects_the_257th() {
        let (mut handle, mut pump) = runtime();
        for id in 0..MAX_ACTIVE_AUDIO_CUES_PER_ENDPOINT as u128 {
            handle.try_send(add(id + 100, 1)).unwrap();
        }
        handle.try_send(add(10_000, 1)).unwrap();

        let mut processed = 0;
        let mut applied = 0;
        let mut rejected = 0;
        while processed < 257 {
            let report = pump.process_pending_commands();
            assert!(report.processed <= MAX_COMMANDS_PER_FRAME);
            processed += report.processed;
            applied += report.applied;
            rejected += report.rejected;
        }

        assert_eq!(processed, 257);
        assert_eq!(applied, 256);
        assert_eq!(rejected, 1);
        assert_eq!(pump.active_count(), 256);
        assert_eq!(pump.occupied_count(), 256);
        assert_eq!(pump.take_last_error(), Some(SlotError::NoFreeSlot));
    }

    #[test]
    fn updates_and_arm_are_rejected_after_a_cue_enters_retirement() {
        let (mut handle, mut pump) = runtime();
        let cue = key(14, 1);
        handle.try_send(add(14, 1)).unwrap();
        handle
            .try_send(SparseMixerCommand::Remove {
                key: cue,
                reason: RemoveReason::Stop,
            })
            .unwrap();
        handle
            .try_send(SparseMixerCommand::UpdateGain {
                key: cue,
                revision: 1,
            })
            .unwrap();
        handle
            .try_send(SparseMixerCommand::Arm {
                key: cue,
                transaction: LaunchTransactionId(14),
                start_frame: 14,
            })
            .unwrap();

        let report = pump.process_pending_commands();
        assert_eq!(report.applied, 2);
        assert_eq!(report.rejected, 2);
        assert!(pump.is_retiring(cue));
        assert_eq!(pump.take_last_error(), Some(SlotError::CueRetiring));
    }
}
