//! Lightweight system performance sampling for the transport bar.
//!
//! CPU sampling is performed once per second and never runs in the audio
//! render callback. On Windows, GetSystemTimes provides the machine-wide load.

use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct SystemCpuMonitor {
    last_sample_at: Instant,
    previous_times: Option<(u64, u64)>, // (idle, total)
    usage_percent: Option<f32>,
}

impl SystemCpuMonitor {
    pub fn new() -> Self {
        Self {
            last_sample_at: Instant::now(),
            previous_times: read_cpu_times(),
            usage_percent: None,
        }
    }

    /// Refresh the system-wide CPU percentage at most once per second.
    pub fn update(&mut self) {
        let now = Instant::now();
        if now.saturating_duration_since(self.last_sample_at) < Duration::from_secs(1) {
            return;
        }
        self.last_sample_at = now;

        let Some(current) = read_cpu_times() else {
            self.usage_percent = None;
            self.previous_times = None;
            return;
        };

        if let Some(previous) = self.previous_times {
            let idle_delta = current.0.saturating_sub(previous.0);
            let total_delta = current.1.saturating_sub(previous.1);
            if total_delta > 0 {
                let busy_delta = total_delta.saturating_sub(idle_delta);
                self.usage_percent =
                    Some((busy_delta as f64 / total_delta as f64 * 100.0).clamp(0.0, 100.0) as f32);
            }
        }
        self.previous_times = Some(current);
    }

    pub fn usage_percent(&self) -> Option<f32> {
        self.usage_percent
    }
}

#[cfg(target_os = "windows")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileTime {
    low_date_time: u32,
    high_date_time: u32,
}

#[cfg(target_os = "windows")]
impl FileTime {
    fn as_u64(self) -> u64 {
        ((self.high_date_time as u64) << 32) | self.low_date_time as u64
    }
}

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
extern "system" {
    fn GetSystemTimes(
        idle_time: *mut FileTime,
        kernel_time: *mut FileTime,
        user_time: *mut FileTime,
    ) -> i32;
}

#[cfg(target_os = "windows")]
fn read_cpu_times() -> Option<(u64, u64)> {
    let mut idle = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    let success = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
    if success == 0 {
        return None;
    }

    // Windows kernel time already includes the idle time.
    let idle = idle.as_u64();
    let total = kernel.as_u64().saturating_add(user.as_u64());
    Some((idle, total))
}

#[cfg(target_os = "linux")]
fn read_cpu_times() -> Option<(u64, u64)> {
    let contents = std::fs::read_to_string("/proc/stat").ok()?;
    let cpu_line = contents.lines().find(|line| line.starts_with("cpu "))?;
    let values: Vec<u64> = cpu_line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|value| value.parse::<u64>())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if values.len() < 4 {
        return None;
    }

    let idle = values[3].saturating_add(*values.get(4).unwrap_or(&0));
    let total = values.iter().copied().sum();
    Some((idle, total))
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn read_cpu_times() -> Option<(u64, u64)> {
    None
}
