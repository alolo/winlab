# Theatre Show Cue 0.9.0 — Stage 3.2.2 Test Plan

## Build and unit tests

Run in PowerShell from the project folder:

```powershell
cargo fmt
cargo check --locked
cargo test --locked
cargo build --release --locked
```

## TASCAM native mapping

1. Select `ASIO — US-322 / US-366`.
2. Open Settings → Audio → Logical Output Mapping.
3. Confirm the default mapping is:
   - TaSQ Output 1 = `1L · ASIO 1`
   - TaSQ Output 2 = `2R · ASIO 2`
   - TaSQ Output 3 = `3L · ASIO 3`
   - TaSQ Output 4 = `4R · ASIO 4`
   - TaSQ Output 5 = `5L · ASIO 5`
   - TaSQ Output 6 = `6R · ASIO 6`
4. Confirm Default Stereo Pair follows TaSQ Output 1/2 (the first two rows of Logical Output Mapping), not a driver-detected LINE OUT pair.
5. Press Test on Outputs 1–6 one at a time. Confirm the TASCAM Mixer Panel shows the Computer bus assigned to each mapped ASIO channel.
6. Import a stereo audio file into a brand-new empty project without manually changing its routing. Confirm it uses the same physical channels as Test on TaSQ Output 1/2.
7. Change the logical mapping so TaSQ Output 1/2 point to a different ASIO pair. Confirm Default Stereo Pair updates to that new pair and a newly imported stereo Cue follows it.
8. Confirm the optional `Use detected pair` action still works when an operator intentionally wants the driver's detected physical pair.

## Preserve logical Cue assignments when changing the map

1. Create a stereo Cue routed to TaSQ Output 1/2 and a multichannel Cue routed to several logical outputs.
2. Note the destination labels in each Cue's I/O Routing matrix.
3. Change the Logical Output Mapping (for example, swap TaSQ Output 1 with TaSQ Output 3).
4. Confirm the Cue matrices still show the same logical TaSQ Output destinations, while their native ASIO labels change to match the new permutation.
5. If a Cue is playing, confirm its live routing changes without restarting playback.
6. Confirm Default Stereo Pair follows the first two logical outputs after the map change.
7. Reset to native driver order and confirm the same logical Cue destinations remain intact and the default stereo pair returns to native Output 1/2.

## Legacy project migration

1. Open a project saved by a pre-Stage-3.2.2 build while the TASCAM ASIO endpoint is active.
2. Confirm its old TASCAM logical routes are translated once to the corrected native order.
3. Save, close and reopen the project.
4. Confirm the routes are not translated a second time and the project remains on version 2.
5. Repeat after starting TaSQ on another backend and switching to TASCAM before opening the legacy project.

## Stereo routing warning

1. Open an Audio Cue with a stereo source.
2. Route Left and Right to the same output.
3. Confirm a yellow warning appears in I/O Routing explaining that the signals are summed on that shared output.
4. Route Left and Right to separate outputs and confirm the warning disappears.
5. Confirm intentional mono downmix is still allowed.

## Cue List unavailable-output styling

1. Create an Audio Cue with active output routes and confirm there is no red outline.
2. Disable every destination in its routing matrix. Confirm the row gets the same thin red outline as a Cue with a missing audio file.
3. Hover the row and confirm the tooltip explains that no active hardware output is assigned.
4. Assign an explicit endpoint, disconnect it, and confirm the same red outline and an unavailable/disconnected tooltip appear.
5. Assign an endpoint on another backend or a non-active ASIO driver and confirm the row explains why the route cannot currently play.
6. Confirm a Cue using Global Output is not marked as broken while the active global stream is available.
7. Confirm non-audio Cues are not marked as having missing audio outputs.

## Buffer size options

### WASAPI Shared

1. Select a WASAPI output.
2. Confirm the menu offers Automatic plus 256, 512, 1024, 2048, 4096 and 8192 frames.
3. Try 512, 1024 and 2048 frames at 48 kHz.
4. Confirm the stream reopens and audio remains stable.
5. Remember: these values request shared ring-buffer capacity. Windows controls the callback period; actual latency can differ from the displayed frame-duration estimate.

### DirectSound

1. Select DirectSound.
2. Confirm the menu offers Automatic (4096 frames) plus 1024, 2048, 4096, 8192 and 16384 frames.
3. Compare 4096, 8192 and 16384 during continuous playback.
4. Record clicks/dropouts and subjective response latency.

### ASIO

1. Select TASCAM ASIO.
2. Confirm only driver-reported block sizes appear. If the driver reports no usable range, only Automatic should be offered.
3. Change the block size in the TASCAM control panel, then reopen/reselect the ASIO endpoint in TaSQ if required.
4. Confirm the active stream retains six output channels.

## Regression checks

- Verify Test tone and new stereo Cue playback use the same native channels for TaSQ Output 1/2, including after restarting TaSQ with existing settings.
- Verify logical mapping changes do not change source channel count or collapse stereo unexpectedly.
- Verify output channel mute/trim settings remain attached to physical hardware outputs.
- Verify sample-rate selection, Cue pause/resume/seek/fade/stop, and endpoint switching.
- Confirm no dependency changes and no Stage 4 worker-process changes.

## Report back

For each backend, report the selected endpoint, sample rate, selected buffer value, whether the stream opened, and whether clicks/dropouts occurred. Include `audio.log` and the application log if an endpoint or route is rejected.
