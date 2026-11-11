# Stage 3.2.1 — UI test plan

1. Build:
   ```powershell
   cargo fmt
   cargo check --locked
   cargo test --locked
   cargo build --release --locked
   ```

2. Transport bar:
   - With the lower panel visible, verify `Hide panel` appears at the far right after Master Volume.
   - Click it and verify the lower panel hides.
   - Verify the same button remains in the same position and changes to `Show panel`.
   - Click it and verify the lower panel returns.
   - Confirm GO, Stop/Pause/Resume selected/all and Master Volume still work.

3. Continue column:
   - Check `Nothing`, `Auto Continue` and `Auto Follow` in several rows.
   - Verify the selected value is centered in the closed control.
   - Open the dropdown and verify all options are centered.
   - Change each value and confirm the selection still saves and behaves as before.
