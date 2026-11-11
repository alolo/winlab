# Stage 3.1.1 — Build hotfix

- Fixed an accidentally literal `\\n` sequence between two `use` declarations in `src/audio/engine.rs`; the declarations are now on separate source lines.
- Added an explicit `AudioEngine` type annotation and normalized the `match` formatting in the global endpoint switch to avoid never-type fallback inference on supported Rust toolchains.

# Theatre Show Cue 0.9.0 — Stage 3: Device Registry

## Stage 1 — Backup & Logs

- Renamed the settings tab to `Backup & Logs`.
- Moved diagnostic log destination settings out of Audio.
- Retained separate audio and application logs.
- Keeps up to 30 audio logs, 30 application logs and 30 diagnostic reports.
- Added live retained-file counts and ZIP diagnostic export.
- ZIP export includes only Theatre Show Cue log/report files and `LATEST.txt`; it excludes project files, audio media and unrelated files.
- Project backups retain the 10 newest snapshots.

## Stage 2 — Routing & Settings

- Removed the separate Audio Driver selector from Global Audio Settings and replaced it with one combined backend/device selector:
  - `ASIO — device name`
  - `WASAPI — device name`
  - `DirectSound — device name`
- The selected backend and device are persisted together using the existing settings fields (`audio_backend` + `audio_output_device`) for compatibility.
- Startup opens the saved endpoint directly instead of opening a backend default first and switching afterward.
- Device switching within ASIO releases the previous stream before attempting to open another ASIO driver. If the new driver fails to open, the worker attempts to restore the previous ASIO device.
- Cue routing stores the backend alongside an explicit output-device name.
- Older projects without `output_backend` continue to load; their explicit device routes are interpreted as belonging to the active global backend.
- Every listed backend/device combination can be selected and saved per Cue.
- Until Stage 4 adds worker processes, playback is allowed only when the Cue's route backend matches the active global backend. ASIO additionally permits only the currently active ASIO device in this process.
- Incompatible routes show a clear status/log message instead of silently using the wrong output.
- When the global backend changes, legacy explicit Cue routes are pinned to the backend they previously used rather than being silently reassigned.
- Changing a Cue's output endpoint stops that Cue so it cannot continue playing through the previous device by mistake.

## Stage 3 — Device Registry

- Added `src/audio/device_registry.rs` as the central cross-backend output endpoint registry.
- WASAPI and ASIO use CPAL 0.18.2 stable `DeviceId` values for persistent endpoint identity. DirectSound uses a normalized backend/name ID because the SDL2 API used here does not expose the same CPAL native ID.
- The registry stores backend, stable ID, display/native name, channel count, supported channel layouts and availability state.
- Device discovery runs on a background thread; the UI thread consumes scan snapshots without blocking on device enumeration.
- The registry performs a full initial scan, automatic five-second WASAPI/DirectSound hot-plug polling and a manual `Refresh devices` action in Global Audio Settings.
- The ASIO driver inventory is loaded once at startup and is not re-enumerated while the app is running, avoiding repeated initialization of vendor drivers. Restart the app after installing/removing ASIO drivers.
- Missing endpoints are retained as `Unavailable` rather than deleted. Reconnecting the same stable ID restores the existing registry entry and saved Cue route.
- Stable IDs are persisted for the global output endpoint in application settings and for each explicit Cue output route. Old settings and projects without IDs remain supported and migrate from the backend/name pair when the device is discovered.
- A display-name change does not break a CPAL route if the stable device ID remains the same.
- If the active global output disappears, playback is stopped and the route is retained. After reconnection, selecting that endpoint again rebuilds its audio stream.
- New Cue playback is blocked with a clear warning if its explicit endpoint is offline; the application does not silently redirect it to another device.
- Background scans enumerate IDs/names only. Channel capability probing remains on the active/selected backend path to avoid repeatedly probing every ASIO driver while audio is running.

## Tests added

- Legacy I/O Routing JSON deserializes without backend or stable endpoint ID fields.
- Legacy application settings deserialize without `audio_output_device_id`.
- Combined backend labels use the expected short names.
- Device Registry labels show backend and offline state.
- Stable IDs survive display-name changes.
- Disconnected endpoints are retained and reconnected by ID.
- Failed scans do not falsely mark devices offline.
- Lightweight scans preserve previously probed channel metadata.

## Build and verification

Run on Windows with the project's Rust toolchain:

```powershell
cargo fmt
cargo check --locked
cargo test --locked
cargo build --release --locked
```

No new Cargo dependencies were added. This environment does not include Rust/Cargo, so compilation and Rust unit tests must be run on the Windows development machine.

## Deliberate Stage 3 limitation

The registry manages output endpoints. Simultaneous playback through different backends and multiple ASIO drivers is still not enabled. A Cue routed to a non-active backend or a non-active ASIO driver is stopped with a clear diagnostic message. Stage 4 will replace this limitation with isolated audio workers.


## Stage 3.1 — Audio routing and UI hotfix (2026-11-11)

### Global backend switching and Device Registry
- Backend switches now reuse the current Device Registry snapshot instead of rescanning every backend while the previous ASIO stream is still open. This prevents transient ASIO enumeration failures from removing ASIO entries from the global device selector.
- Fixed registry scan merging: an unchanged endpoint is no longer marked disconnected and reconnected on every five-second scan. Only IDs actually absent from a successfully completed backend scan are marked unavailable.
- Added a regression test for identical repeated scans.

### DirectSound stability
- Replaced SDL2 callback-based DirectSound output with SDL2's queue API.
- The queue is primed with silence and refilled from the Rodio mixer by the audio worker. This removes the SDL2 Rust callback path that panicked at `sdl2-0.38.0/src/sdl2/audio.rs:726` in the supplied diagnostics; the exact internal `Option` that panicked is not assumed.

### Output test signal
- Test signal is now added directly to the active output mixer's controller, with explicit interleaved native-channel isolation.
- Replaced abrupt `Player::stop()` with a finite 900 ms source, 8 ms fade-in/out, and generation-based smooth cancellation when another channel is tested.
- Logs now include backend, endpoint name, generation, logical output, native stream channel, stream channel count and sample rate.
- Added unit coverage for channel isolation and cancellation fade.

### Cue List columns
- All visible Cue List columns now scale proportionally to the available width. The previous implementation assigned all extra width to Name and shrank a fixed subset of columns, which made resizing/window scaling appear inconsistent.
- Drag resizing still changes the saved width proportions and preserves the existing nine-value settings format.

### Verification
- Static source consistency and archive integrity checks passed.
- Rust compilation and unit tests were not run in this environment; run them on Windows:

```powershell
cargo fmt
cargo check --locked
cargo test --locked
cargo build --release --locked
```

### Stage 4 boundary
- Per-Cue simultaneous playback across different backends remains a Stage 4 feature. Global backend switching (ASIO ↔ WASAPI ↔ DirectSound) is expected to work before Stage 4 and is addressed by this hotfix.
