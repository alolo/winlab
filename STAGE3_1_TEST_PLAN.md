# Theatre Show Cue 0.9.0 — Stage 3.1 Hotfix Test Plan

## Build

```powershell
cargo fmt
cargo check --locked
cargo test --locked
cargo build --release --locked
```

## 1. Global output switching

- Start with the ASIO output selected.
- Open Global Settings → Audio.
- Switch to `DirectSound — Динамики (TASCAM US-366)`.
- Confirm the selected endpoint and active backend change, with no SDL panic.
- Switch back to the TASCAM ASIO endpoint.
- Confirm ASIO entries remain in the selector after each switch and after several five-second registry scans.

Expected: global switching works independently of Stage 4. Stage 4 is only required for simultaneous per-Cue playback across different backends.

## 2. Device Registry / hot-plug

- Leave the app open for at least 30 seconds without changing devices.
- Confirm logs do not report repeated disconnect/reconnect events for unchanged devices.
- Disconnect one USB audio interface.
- Confirm only that endpoint becomes `Unavailable`.
- Reconnect it and press `Refresh devices` if needed.
- Confirm the same stable endpoint ID becomes available again.

Expected: unchanged devices do not flap between Available and Unavailable on each scan.

## 3. Cue List columns

- Resize the main window narrower and wider.
- Confirm Cue, Type, Name, File/Target, timing and Continue columns all scale together.
- Drag a column divider.
- Resize the window again and confirm the saved column proportions remain.

## 4. Output test signal

- Select the TASCAM ASIO endpoint.
- Use the existing Logical Output Mapping.
- Test each logical output separately.
- Check `audio_*.log` for `OUTPUT TEST` entries: backend, endpoint ID, logical output, native stream channel, stream channel count and sample rate.
- Confirm the signal is audible only on the selected native channel.

Expected: repeated Test clicks fade out the previous test instead of abruptly stopping it. The source itself is a 440 Hz tone, 900 ms long, with an 8 ms fade.

If a separate short beep remains on physical outputs 1/2 while the log reports a different native channel, capture a short phone recording and keep the matching audio/application logs. That would help distinguish an application-generated test signal from a driver/device-side transient.

## 5. Per-Cue cross-backend routing

- A Cue may save a route to a backend different from the active global backend.
- It will not play through that different backend until Stage 4.

This is an intentional Stage 3 limitation.
