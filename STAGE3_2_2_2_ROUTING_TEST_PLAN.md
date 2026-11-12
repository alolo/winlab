# Stage 3.2.2.2 — Logical routing validation

## Windows build

```powershell
cargo clean
cargo fmt
cargo check --locked
cargo test --locked
cargo build --release --locked
```

## Device-neutral baseline

1. Start a new, empty TaSQ project.
2. Select `ASIO — US-322 / US-366` (or any other multichannel ASIO device).
3. In Settings → Audio, select `Reset to native driver order` in Logical Output Mapping.
4. Leave Default Stereo Pair at TaSQ Output 1/2. Import a stereo file.
5. Open I/O Routing and confirm the new Cue is assigned to TaSQ Output 1/2.
6. Play the file, then trigger Test for TaSQ Output 1 and 2. The driver channels receiving the file and each Test must coincide.
7. Check `audio.log`: Test reports `logical_output` and `mapped_native_channel`; playing an Audio Cue reports `logical_routes` and `native_routes`.

## Mapping change must not rewrite a Cue

1. With an existing Cue assigned to TaSQ Output 1/2, swap the native destinations for TaSQ Output 1/2 and TaSQ Output 5/6 in Logical Output Mapping.
2. Confirm the Cue still displays *TaSQ Output 1/2* in I/O Routing. Default Stereo Pair must remain *TaSQ Output 1/2*.
3. Play the Cue and Test TaSQ Output 1/2. They must now use the newly selected native channels, consistently.
4. Change Default Stereo Pair to TaSQ Output 3/4. Existing Cue routing must remain unchanged.
5. Import a second stereo file. Only the new Cue should default to TaSQ Output 3/4.
6. Change Logical Output Mapping again. Both Cues must retain their respective logical I/O assignments (1/2 and 3/4).
7. Repeat with GIGAPORT or Sound Blaster to verify no device-specific path exists.

## Important distinction

TaSQ uses the native **ASIO channel indices** reported by CPAL. The TASCAM driver/mixer may expose a separate Computer Bus / monitor routing model. TaSQ does not hardcode a guessed translation between those layers. `Logical Output Mapping` controls TaSQ logical slots → native driver channel only.

## Reporting

For each failed case, include the exact `Logical Output Mapping`, Default Stereo Pair, the Cue's I/O Routing assignment, and the matching Audio log entries (`Audio Cue routing` and `OUTPUT TEST`).
