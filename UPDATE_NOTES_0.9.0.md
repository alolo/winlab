# TaSQ 0.9.0 — Update notes

## Stage 4.4.1 — Worker health and shutdown safety

- Retained per-worker heartbeat monitoring and event-stream failure detection.
- Added per-endpoint Windows Job Objects with kill-on-close protection.
- Bounded worker process shutdown: 2 seconds graceful wait plus at most 500 ms forced-exit polling.
- Removed blocking `Child::wait()` calls from worker process teardown and startup-failure cleanup.
- Added a 3-second deadline for the GUI-side worker manager; remaining worker processes are force-terminated and the manager receives a further 1-second grace period.
- Shutdown commands use nonblocking queue sends to prevent a saturated IPC queue from stalling manager teardown.
- Added endpoint/backend/device/PID shutdown logging.
- Added automated tests for bounded shutdown constants and safe empty-supervisor termination.
- No dependency changes, no Cargo.lock changes, and no IPC protocol change (protocol remains version 2).
- Audio routing, Cue state, sample-rate handling and normal playback behavior are unchanged.


## Stage 4.4.1.1 — Compile fix

- Removed an accidentally supplied third argument from one `apply_saved_endpoint_output_settings` call. The helper only applies saved channel settings to an already-running worker and does not need the job supervisor.
- No runtime behavior, worker supervision, shutdown logic, dependencies, or lockfile changes.
