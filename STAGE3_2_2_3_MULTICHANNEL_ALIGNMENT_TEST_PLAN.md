# Stage 3.2.2.3 — Multichannel Alignment Test Plan

## Build and unit tests

```powershell
cargo clean
cargo fmt
cargo check --locked
cargo test --locked
cargo build --release --locked
```

The regression test `player_queue_preserves_six_channel_frame_alignment_from_first_sample` uses a six-channel source with a distinct value on each channel and verifies that the mixer output preserves the exact interleaved sequence.

## TASCAM ASIO verification

1. Select TASCAM ASIO with six output channels.
2. Set Logical Output Mapping to a non-identity permutation, for example TaSQ 1/2 -> ASIO 3/4 and TaSQ 5/6 -> ASIO 1/2.
3. Stop all Cue playback before changing the mapping.
4. Test TaSQ Output 1, 2, 5 and 6 individually.
5. Import a stereo file into a new empty project and leave its I/O Routing at TaSQ Output 1/2.
6. Play the file. It must reach the exact same TASCAM Computer Bus pair as Test Output 1/2.
7. Reset the mapping to native order, stop and restart the Cue, and repeat.
8. Repeat with a four-channel output device if available.

## Log evidence to check

- `Output test requested`: logical output and mapped native channel.
- `Audio Cue routing`: logical routes and native routes.
- `ChannelRouter configured`: source channel count and output channel count.
- Cue playback now attaches its first queued source only after the final multichannel layout is known.

## Additional routing sanity check

If the log shows routes such as `output_channels: [0, 4]` and `[1, 5]`, the Cue is intentionally or accidentally fanning stereo Left/Right to TaSQ Outputs 1/5 and 2/6. For a simple stereo comparison, the expected routes are `[0]` and `[1]`.
