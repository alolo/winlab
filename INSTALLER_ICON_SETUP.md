# TaSQ Windows installer icon

The application executable receives the multi-resolution icon from `assets/TaSQ_multi.ico` during the Windows build.

To use the same artwork for the Inno Setup installer, add this line inside `[Setup]` in `TaSQ.iss`:

```ini
SetupIconFile=assets\TaSQ_multi.ico
UninstallDisplayIcon={app}\TaSQ.exe
```

Keep the existing `UninstallDisplayIcon` line if it is already present; only update its path if needed.
