# CueLabs — Settings Popups, Waveform Scrollbar, Fade and I/O Routing

## Changes in this revision

### 1. Settings window and popups
- Moved the modal input blocker to the middle UI layer.
- Moved the Settings window to the foreground layer so ComboBox popups and device selectors render above the blocker and window contents.
- The blocker still prevents interaction with the workspace underneath Settings.

### 2. Waveform ruler and horizontal navigation
- Reduced the waveform timecode label font to 10 px while retaining white text.
- Replaced the narrow slider with a full-width custom scrollbar below the waveform.
- The scrollbar spans the editor viewport, keeps a rounded thumb, supports click-to-page and drag-to-pan, and does not change zoom.

### 3. Continue ComboBox selection protection
- Row selection's manual pointer fallback now checks the layer under the pointer.
- A click on an open Continue ComboBox popup cannot select the row underneath or start inline editing.

### 4. Multichannel output discovery
- Output channel layouts are collected from every supported CPAL output configuration, deduplicated and sorted.
- The engine selects the supported configuration with the greatest channel count, preferring F32 only when channel counts are equal.
- Device layouts are refreshed when a device is selected, so changed Windows endpoint settings are not stuck in the startup cache.
- Settings and I/O Routing now display the channel layouts actually reported by the driver.
- If the driver exposes only stereo, Settings displays a note explaining that Windows/device multichannel or ASIO configuration may be required.

Important limitation: CueLabs can only open channel counts that the active CPAL host/driver exposes. If Windows reports only `2 ch` for an interface with four physical outputs, the application cannot safely create four independent outputs through that endpoint. Configure the Windows speaker layout or the manufacturer's multichannel/ASIO driver mode, then reselect the device. The new diagnostics make this distinction visible.

### 5. Fade behavior and status
- A Fade In whose target cue is muted at -60 dB (linear zero) now defaults to 0 dB unless an explicit target level is configured.
- Fade In still respects `Continue = Nothing`: it does not start or resume a stopped/paused target in that mode.
- Fade Out only acts on a target that is already playing; it never starts a stopped target.
- A Fade event's status cell shows Fading In / Fading Out while the operation is running, then returns to Stopped on completion.
- Fade Out is represented as a descending curve in the preset preview and custom curve editor.
- Fade Continue timing includes the fade duration plus Pre-Wait and Post-Wait.
- Automatic continuation skips the Fade's own target when that target is the next cue (or is inside the next group), preventing a Fade Out from launching the event it is fading.

### 6. I/O gain editing
- Added editable dB fields beside hardware-output and track-route gain sliders.
- Typing a lone minus sign and committing it sets the gain to -60 dB.
- Double-clicking the numeric dB field resets it to 0 dB.
- Numeric values are clamped to -60..+18 dB.

## Static verification
- All Rust files were checked for UTF-8 decoding and balanced delimiters.
- Updated call sites for Fade curve previews/editors were checked.
- Settings layer ordering, channel-layout diagnostics, Continue popup protection and Fade continuation logic were checked in source.
- This preparation environment has no Rust toolchain, so `cargo check` and `cargo test` were not run here. Please run them on Windows before using the build.
