# Theatre Show Cue 0.8.0 — update notes

## Diagnostic logging

The application creates separate session files:

- `audio_YYYYMMDD_HHMMSS_mmm.log` — audio hosts, detected output/input devices, supported channel layouts, selected stream configuration, stream-open attempts/failures, output routing, device switches and output tests.
- `application_YYYYMMDD_HHMMSS_mmm.log` — cue triggers and execution, GO/STOP/PAUSE/RESUME, waveform seeks, project open/save/import actions, backend changes and application commands.
- `diagnostic_report_YYYYMMDD_HHMMSS_mmm.txt` — OS, architecture, logical CPU count, build profile, audio backend and enumerated audio devices.
- `LATEST.txt` — names of the most recent session files.

Default folder on Windows:

`%LOCALAPPDATA%\\TheatreShowCue\\logs`

The destination can be changed in **Settings → Audio → Diagnostic logging**. A selected folder is saved in application settings and applies immediately. If the configured folder is unavailable at startup or disappears during a session, the application falls back to the default location.

Automatic retention keeps at most 30 files of each diagnostic type (audio, application and report). Only files matching the application's diagnostic filename prefixes are removed; unrelated files are left untouched.

The logs contain technical metadata such as device names, cue names, and selected file/project paths. They do not record audio, microphone input, Notes text, or raw MIDI payloads.

## Waveform and paused position

- Waveform resolution increased from 512 to 4096 min/max buckets.
- Existing project waveforms with fewer than 4096 buckets are regenerated when media metadata is checked/opened.
- The playhead/seek position is drawn as a yellow vertical marker while paused or parked.
- The Cue List Duration cell displays the frozen position (for example `⏸ 00:15.24`) while a cue is paused, and also displays a parked waveform-seek position before playback.

## Audio backends

ASIO is now compiled as part of the single standard Windows build. The project no longer defines a separate `asio` Cargo feature. WASAPI remains available and uses CPAL's Windows default host; ASIO is selected explicitly through CPAL's ASIO host.

## Build and verification

Use the standard build without a project feature flag:

```powershell
cargo fmt
cargo check --locked
cargo test --locked
cargo build --release --locked
```
