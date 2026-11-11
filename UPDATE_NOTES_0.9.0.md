# Theatre Show Cue 0.9.0 — Update Notes


## Stage 3.2.1 — Transport & Cue List UI polish

- Moved `Show panel` / `Hide panel` into one toggle at the far right of the transport bar, immediately after Master Volume.
- Removed the previous duplicate visibility controls from the lower-panel tab strip and project/cue header.
- Centered the selected Continue value (`Nothing`, `Auto Continue`, `Auto Follow`) inside its control.
- Centered the Continue choices in the dropdown popup.
- No audio engine, routing, project format, or dependency changes.

## Stage 3.2 — Audio Performance Settings

- Added global Sample Rate and Buffer Size controls to Settings → Audio.
- Settings are persisted in `settings.json`; older settings files migrate automatically with both values set to Automatic.
- CPAL backends (WASAPI and ASIO) request the selected sample rate and `cpal::BufferSize::Fixed(frames)` through Rodio's `DeviceSinkBuilder::with_buffer_size`.
- The CPAL sample-rate list is limited to rates supported by the device's maximum channel layout, so changing the rate does not silently reduce a multichannel interface to stereo.
- CPAL buffer choices are filtered against the driver's reported supported range when available. A rejected fixed size produces an error and the previous active stream/settings are retained.
- DirectSound uses the selected sample rate for its SDL device and maps Buffer Size to the application-managed SDL queued-audio safety window.
- DirectSound's Automatic buffer is 4096 frames. Available queue targets: 1024, 2048, 4096, 8192 and 16384 frames.
- DirectSound queue refills now use a per-device target and half-buffer low-water mark rather than a fixed 2048/1024 frame pair.
- The active sample rate is displayed in Settings. Buffer duration is shown as a single-buffer estimate; it is not a measurement of end-to-end latency.
- Changing either setting reconfigures the active stream in its existing audio worker: the old native stream is released before the same endpoint is reopened. This avoids trying to open a second copy of an active ASIO driver.
- If the requested settings cannot be opened, the worker attempts to restore the previous performance profile and the UI restores the previous setting.
- Switching to a different endpoint that does not support the current global performance profile retries with Automatic settings and updates the saved preferences accordingly.
- No Cargo dependencies were added; `Cargo.toml` and `Cargo.lock` are unchanged.

### Stage 3.2 verification

Windows build and runtime verification are still required. See `STAGE3_2_AUDIO_PERFORMANCE_TEST_PLAN.md`.

## Warning cleanup — unused audio constructor

- Removed the unused `new_with_backend_and_endpoint_using_registry` convenience wrapper.
- Kept `new_with_backend_and_endpoint_using_registry_and_performance`, which is used by the active endpoint-switch path.
- No runtime behavior, audio settings, project format, or dependencies changed.

