# CueLabs patch notes

## Fixed

1. Updated all seven audio-device display/lookup sites in `src/audio/engine.rs` for CPAL 0.18.2.
   - CPAL 0.18 removed `DeviceTrait::name()`.
   - The code now uses `Device::to_string()` for the same user-facing device-name string.
   - Applied to default input/output names, input/output device lists, channel-layout refresh, and per-cue output-device lookup.
2. Removed the unused `cues_len` local in `src/ui/cue_table.rs`.
3. Removed the unused `HashMap` test-module import in `src/app.rs`.

## Validation status

- Confirmed no `.name()` calls remain under `src/`.
- Confirmed `Cargo.lock` pins CPAL 0.18.2.
- Confirmed the archive contains the complete project source and Cargo manifests.
- `cargo check` / `cargo test` were not run in this environment because Rust/Cargo are not installed here. Run both on the Windows development machine after extracting this archive.
