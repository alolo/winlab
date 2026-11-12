# Theatre Show Cue 0.9.0 — Update Notes

## Stage 3.2.2 — Audio Routing & Buffer Fix

### TASCAM US-322 / US-366 routing

- Corrected the default logical output order to native ASIO order: ASIO 1 → Computer 1, ASIO 2 → Computer 2, through ASIO 6 → Computer 6.
- Corrected TASCAM channel labels to match that verified order: 1L/2R, 3L/4R, 5L/6R.
- By default, the stereo pair for new Cues follows the first two logical TaSQ outputs. Driver-detected physical Line Out pairs remain available only as an explicit manual override.
- Logical Output Mapping changes now preserve the logical destinations of existing Cue routes by translating old native indices through the old and new logical maps.
- The default stereo pair follows the first two logical outputs after a mapping change. A separately selected custom pair remains an explicit override until the next mapping change.
- Driver-detected stereo pairs are no longer silently selected for new Cue playback; they remain available through the explicit `Use detected pair` action.
- Updated currently playing Cue routing immediately when the mapping changes; no audio restart is required.
- Added a one-time per-device migration record for legacy TASCAM projects/settings. Version 1 projects are translated to the corrected map when opened with the matching TASCAM ASIO endpoint. New projects use project version 2.
- Existing user-defined logical permutations are retained; only the old built-in TASCAM permutation is replaced by native order.

### Routing validation and Cue List status

- Added a non-blocking I/O Routing warning when stereo Left and Right are both sent to the same hardware output. The warning explains that the signals are summed on that output; intentional mono folds remain allowed.
- Audio Cues with no active hardware destination, an unavailable/disconnected explicit endpoint, an inactive cross-backend route, or a non-active ASIO driver receive the same thin red row outline used for missing audio media.
- Hovering an affected row shows the reason. A Cue using the Global Output is considered assigned when the active global stream is available.

### Buffer size controls

- WASAPI Shared now offers 256, 512, 1024, 2048, 4096 and 8192 frame requests instead of restricting the menu to the single Windows-reported default period.
- On WASAPI Shared, the selected value requests the shared ring-buffer capacity. Windows still controls the callback period; the requested capacity is not a promise of a matching callback period or end-to-end latency.
- DirectSound offers 1024, 2048, 4096, 8192 and 16384 frame application queue targets.
- ASIO shows only buffer sizes reported by the driver. If the driver does not expose a range, Automatic is the only offered setting; hardware block size remains configured in the vendor control panel (for example, TASCAM Settings).
- Stale saved sizes from another backend are reset to Automatic for ASIO and DirectSound rather than being shown as unsupported choices.
- Backend-specific help text now explains what each buffer control actually changes.
- No Cargo dependencies were added; Cargo.toml and Cargo.lock are unchanged.

### Stage 3.2.2 verification

Static source checks and archive integrity are performed for this package. Windows compilation and runtime tests must still be run on the target machine; see `STAGE3_2_2_AUDIO_ROUTING_TEST_PLAN.md`.


## Stage 3.2.2.1 — Test / Cue Output Mapping Consistency

- Root cause: the Test buttons resolved each logical output through `Logical Output Mapping`, while new stereo Cues could use `default_output_pairs` populated from a driver-detected physical LINE OUT pair. The two paths could therefore target different ASIO channels even in a brand-new project.
- The Test button now resolves its native channel internally from the logical output index. The UI no longer passes a separately calculated native index.
- New stereo Cues default to the first two native channels in the current Logical Output Mapping, matching Test on TaSQ Output 1/2.
- Driver-detected pairs are informational/optional only; they do not silently override the logical mapping. `Use detected pair` remains available as an explicit operator choice.
- Changing Logical Output Mapping resets the default stereo pair to the first two mapped logical outputs. Existing Cue routes still retain their logical destinations through the old-map → logical-slot → new-map translation.
- Added a one-time per-device TASCAM settings migration that resets stale saved default pairs from previous releases to the first two current logical outputs. The migration is separate from the existing old-project route migration and does not require opening a project.
- Added unit tests proving the default stereo pair and Test channel resolution use the same logical-to-native mapping.
- No audio sample reordering, Cue project migration, dependency, or Stage 4 architecture changes were introduced.

### Stage 3.2.2.1 verification

Static source checks and ZIP integrity are verified. Run the Windows build and tests in `STAGE3_2_2_AUDIO_ROUTING_TEST_PLAN.md` before using the build for a show.

## Stage 3.2.2.2 — Device-neutral logical routing (supersedes previous TASCAM corrections)

- Removed every special TASCAM ASIO channel permutation and Computer Bus label guess from the active audio engine. ASIO and all other backends use the native channel indices reported by CPAL; custom mapping is optional and user-controlled.
- Cue I/O Routing stores *logical TaSQ Output* indices only. `Logical Output Mapping` is applied at the final audio command handoff for both Audio Cues and file-playing Play Cues, including output gains.
- The per-output Test tone resolves its selected logical slot through that same map. There is now one consistent logical-to-native conversion for file playback and Test.
- Changing `Logical Output Mapping` does **not** change Cue channel assignments or `Default Stereo Pair`; the active playback worker receives freshly translated native routing for currently active Cue streams.
- `Default Stereo Pair` is stored as a pair of logical TaSQ Output indices and is used only to initialize routing of new Audio Cues. Choosing TaSQ Output 3/4 stays 3/4 even if the native ASIO channel mapping changes.
- Global device output-mute/gain settings still refer to native channels, while per-Cue Output trim follows the logical TaSQ output slot.
- Generic one-time conversion of old settings' native stereo-pair indices to logical indices (schema marker 3). This is backend-neutral, not a TASCAM workaround.
- Newly saved project files use routing schema version 3. Loading an older project converts its saved native routes to logical slots once; ordinary mapping edits never migrate or modify Cue routing.
- Audio playback logs show *both* the saved logical routes and the effective native ASIO/WASAPI/DirectSound routes. Test logs show logical output and mapped native channel.
- No Cargo dependency changes. Stage 4 worker architecture is not included.

**Testing note:** This project was checked statically; native Windows compilation and a real-device test with the TASCAM mixer panel are still required.

## Stage 3.2.2.3 — Multichannel Player Queue Alignment Fix

- Root cause identified from the latest ASIO logs: test tones were added directly to the six-channel mixer, while Cue playback used `Player::connect_new()`. Rodio attached an empty Player queue before the first six-channel source was appended.
- The empty queue initially reports a mono layout and an artificial 512-sample span. For a six-channel stream, 512 is not frame-aligned (`512 % 6 = 2`), allowing the queue adapter to resume in the middle of an interleaved frame and rotate Cue output channels while direct test tones remain correctly aligned.
- Cue playback now creates `Player::new()`, queues the first fully routed source, and only then attaches the queue to the mixer.
- Added `FrameAlignedPlayerQueue`, which hides the queue's artificial span boundaries for a Cue whose output channel layout remains fixed for its lifetime.
- Added a six-channel regression test using distinct per-channel sample values to detect any interleaved frame rotation.
- Logical Output Mapping and Default Stereo Pair semantics are unchanged. No TASCAM-specific channel permutations or dependency changes were introduced.

