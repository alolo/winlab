# Theatre Show Cue 0.9.0 — Stage 3.2 Test Plan

## Build

Run from the project directory in PowerShell:

```powershell
cargo fmt
cargo check --locked
cargo test --locked
cargo build --release --locked
```

## Sample rate and buffer UI

1. Open Settings → Audio.
2. Confirm the active stream sample rate is displayed.
3. Confirm Sample Rate offers Automatic plus the rates supported by the active device's maximum channel layout.
4. Confirm Buffer Size offers Automatic plus supported/common frame sizes.
5. Select a different supported sample rate. The stream should reopen and the displayed active sample rate should update.
6. Select a different buffer size. The stream should reopen; the setting should survive application restart.
7. Select Automatic for both controls. The backend's automatic/default configuration should reopen.

## ASIO

1. Select the TASCAM ASIO endpoint.
2. Confirm the multichannel layout remains available after changing sample rate.
3. Try a driver-supported buffer size such as 128, 256, 512 or 1024 frames (only values shown by the app).
4. Try an unsupported size if the driver exposes a broad range. The app should report the failure, restore the previous stream in the same worker, and retain the previous setting.
5. Confirm ASIO remains visible in the global endpoint selector.

## WASAPI Shared

1. Select the Focusrite or another WASAPI endpoint.
2. Try 44.1 kHz and 48 kHz when listed.
3. Try 256, 512, 1024 and 2048 frames when listed.
4. Listen for dropouts and note whether the requested settings are accepted. Windows may negotiate an effective period different from the requested callback size.

## DirectSound

1. Select DirectSound — TASCAM or the virtual output.
2. Confirm Automatic reports a 4096-frame queue target.
3. Play continuous audio for at least 5 minutes at 48 kHz / 4096 frames.
4. If clicks remain, try 8192 and then 16384 frames; note the added response latency.
5. For lower latency, compare 2048 frames. Avoid judging a 1024-frame buffer under high CPU load.
6. Try 44.1 kHz and 48 kHz; confirm the actual active rate changes and the output remains stable.
7. Restart the application and confirm the selected rate and queue target are restored.

## Regression checks

- Verify TASCAM's six output channels remain available at every offered ASIO sample rate.
- Verify logical output mapping and per-channel Test still work.
- Verify Cue playback, pause, seek, fade and stop after changing performance settings.
- Verify changing endpoint still works and ASIO does not disappear.
- Check `audio.log` for the requested CPAL buffer, or DirectSound queue frames and estimated queue duration.

## Report back

For each backend, report endpoint name, sample rate, buffer frames, whether the stream opened, and whether clicks/dropouts occurred. Include the relevant audio log if a setting is rejected.
