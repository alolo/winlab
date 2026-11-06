# ASIO setup on Windows

CueLabs now exposes two output backends:

- **WASAPI (Shared)** — Windows shared-mode output.
- **ASIO** — direct output through an installed ASIO driver.

DirectSound and WASAPI Exclusive are not implemented in this build. They require separate Windows audio backends; they are not aliases for WASAPI Shared.

## Build requirements

ASIO is an optional build feature, so the normal WASAPI build does not require Clang. Enable it through CueLabs' `asio` feature (`cpal/asio`). CPAL 0.17.3 requires:

1. Windows x64 and Rust 1.82 or newer.
2. Visual Studio Build Tools with the Desktop development with C++ workload.
3. LLVM/Clang installed.
4. An ASIO driver installed for the audio interface.

Set `LIBCLANG_PATH` to the directory containing `libclang.dll`. A typical LLVM installation uses:

```powershell
setx LIBCLANG_PATH "C:\Program Files\LLVM\bin"
```

Open a new PowerShell window after setting the variable. CPAL can fetch the Steinberg ASIO SDK into its temporary directory during the build. If you already have the SDK in a custom location, set `CPAL_ASIO_DIR` to that directory.

Then rebuild with ASIO enabled:

```powershell
cargo clean
cargo check --features asio
cargo test --features asio
cargo run --release --features asio
```

Without the ASIO feature, the normal WASAPI build remains available:

```powershell
cargo check
cargo run --release
```

## Selecting ASIO

1. Open **Settings → Audio**.
2. Change **Audio driver** to **ASIO**.
3. Select the ASIO driver/device in **Output device**.
4. Check **Active stream outputs** and the channel layout list.
5. Configure per-output Active and gain controls in I/O Routing.

The channel count shown by CueLabs is the channel count exposed by the selected driver to CPAL. A physical connector count printed on a device is not enough by itself: the driver must expose those output channels to the selected backend.

If ASIO fails to initialise, select WASAPI (Shared) again. CueLabs keeps the previous backend active when a backend switch fails.
