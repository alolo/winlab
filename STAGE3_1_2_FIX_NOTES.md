# Theatre Show Cue 0.9.0 — Stage 3.1.2 Hotfix

## 1. Cue List column resizing

- Removed fixed per-column minimum widths.
- Removed the fixed 600 px maximum width.
- Removed the 20 px header-label and 24 px Name-cell floors.
- Name indentation is now included inside the Name column width, so nested Cues do not push later columns out of alignment.
- Resize handles are drawn over column boundaries and do not consume layout width.
- A divider drag redistributes the width of its two neighbouring columns. Either column can shrink to zero; there are no arbitrary pixel caps.
- The shared viewport scaling remains, so the table still fits the available Cue List width.
- Added a unit test proving a column can take the full width of its neighbour pair and shrink to zero.

## 2. DirectSound global output selection

The diagnostic logs show that the global selector did send the requested switch to the audio engine. The failure happened while the DirectSound worker was initializing, not in the settings ComboBox and not because Stage 4 is missing.

The previous implementation used SDL2 wrapper methods that convert `SDL_AudioSpec` by unwrapping the returned sample-format enum. Some DirectSound device specs can contain a format the wrapper does not recognize, causing a panic in the worker thread.

Changes:
- Replaced the DirectSound device-spec query with raw `SDL_GetAudioDeviceSpec`; only channel count and sample rate are read.
- Replaced `sdl2::audio::AudioQueue` construction with a small RAII wrapper around raw SDL2 audio-device functions.
- The requested float32 stream format is validated explicitly before use.
- SDL errors are returned to the application instead of panicking.
- The raw queue still uses SDL2's queue API and is closed automatically when the stream is dropped.
- Existing endpoint discovery, stable IDs, per-device output settings and global backend-switch flow are retained.

## 3. Dependencies and compatibility

- No Cargo dependencies added.
- `Cargo.toml` and `Cargo.lock` unchanged.
- Application version remains 0.9.0.
- Existing Cue List settings array remains backward compatible.

## 4. Windows verification

Run:

```powershell
cargo fmt
cargo check --locked
cargo test --locked
cargo build --release --locked
```

Manual checks:
1. In Global Audio Settings, switch ASIO → DirectSound (TASCAM).
2. Confirm the status reports the selected DirectSound endpoint and the application log contains `DirectSound output opened`.
3. Switch DirectSound → ASIO and confirm ASIO remains in the list.
4. Test playback through the selected DirectSound endpoint.
5. Drag Cue List dividers: shrink Type/Name/File/Target below their previous minimum widths, then expand them again.
6. Confirm nested group indentation does not shift the File/Target and timing columns.

Compilation and Rust tests were not run in the file-generation environment because Cargo/Rustc are unavailable there.
