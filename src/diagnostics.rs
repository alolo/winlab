//! Session diagnostics: separate audio/application logs, bounded retention and
//! a user-selectable destination directory.

use crate::config::AppSettings;
use chrono::Local;
use std::backtrace::Backtrace;
use std::fs::{self, OpenOptions};
use std::io::{self, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

const MAX_LOGS_PER_TYPE: usize = 30;

#[derive(Clone)]
struct LogPaths {
    directory: PathBuf,
    audio: PathBuf,
    application: PathBuf,
    report: PathBuf,
    latest: PathBuf,
}

type SharedLogPaths = Arc<RwLock<LogPaths>>;
static LOG_PATHS: OnceLock<SharedLogPaths> = OnceLock::new();

/// The default per-user location. It is writable without administrator rights.
pub fn default_log_directory() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("TheatreShowCue")
        .join("logs")
}

/// Resolve the saved path, falling back to the standard per-user folder.
pub fn configured_log_directory(saved: Option<&str>) -> PathBuf {
    saved
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_log_directory)
}

/// Return the directory currently used by the live logging writers.
pub fn active_log_directory() -> Option<PathBuf> {
    LOG_PATHS
        .get()
        .and_then(|paths| paths.read().ok().map(|paths| paths.directory.clone()))
}

fn session_log_paths(directory: &Path) -> LogPaths {
    let stamp = Local::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    LogPaths {
        directory: directory.to_path_buf(),
        audio: directory.join(format!("audio_{stamp}.log")),
        application: directory.join(format!("application_{stamp}.log")),
        report: directory.join(format!("diagnostic_report_{stamp}.txt")),
        latest: directory.join("LATEST.txt"),
    }
}

/// Initialize two independent file sinks. Audio-module events go to audio.log;
/// all other application events go to application.log.
pub fn init_logging(settings: &AppSettings) -> anyhow::Result<()> {
    let requested_directory =
        configured_log_directory(settings.diagnostic_log_directory.as_deref());
    let directory = match fs::create_dir_all(&requested_directory) {
        Ok(()) => requested_directory,
        Err(error) => {
            let fallback = default_log_directory();
            eprintln!(
                "Could not create diagnostic log folder '{}': {error}. Falling back to '{}'.",
                requested_directory.display(),
                fallback.display()
            );
            fs::create_dir_all(&fallback)?;
            fallback
        }
    };

    let paths = session_log_paths(&directory);
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.audio)?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.application)?;
    write_basic_report(&paths.report, &directory)?;
    prune_log_files(&directory)?;
    write_latest_file(&paths)?;

    let shared_paths = Arc::new(RwLock::new(paths));
    LOG_PATHS
        .set(shared_paths.clone())
        .map_err(|_| anyhow::anyhow!("Diagnostic logging was already initialized"))?;

    let audio_paths = shared_paths.clone();
    let audio_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_writer(move || open_writer(&audio_paths, true))
        .with_filter(filter_fn(|metadata| is_audio_target(metadata.target())));

    let panic_paths = shared_paths.clone();
    let application_paths = shared_paths.clone();
    let application_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_writer(move || open_writer(&application_paths, false))
        .with_filter(filter_fn(|metadata| !is_audio_target(metadata.target())));

    tracing_subscriber::registry()
        .with(audio_layer)
        .with(application_layer)
        .with(tracing_subscriber::EnvFilter::new(
            "TaSQ=debug,TheatreShowCue=debug,warn",
        ))
        .try_init()
        .map_err(|error| anyhow::anyhow!("Could not install diagnostic subscriber: {error}"))?;

    // Capture panics from the UI thread and audio worker threads in the
    // application log, including a backtrace for tester reports.
    let previous_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let mut writer = open_writer(&panic_paths, false);
        let backtrace = Backtrace::force_capture();
        let _ = writeln!(
            writer,
            "\n=== PANIC ===\n{panic_info}\nBacktrace:\n{backtrace}\n"
        );
        previous_panic_hook(panic_info);
    }));

    Ok(())
}

fn is_audio_target(target: &str) -> bool {
    let target = target.to_ascii_lowercase();
    target == "audio" || target.contains("::audio::") || target.ends_with("::audio")
}

fn open_writer(paths: &SharedLogPaths, audio: bool) -> Box<dyn Write + Send> {
    let path = paths
        .read()
        .map(|paths| {
            if audio {
                paths.audio.clone()
            } else {
                paths.application.clone()
            }
        })
        .unwrap_or_else(|_| std::env::temp_dir().join("TheatreShowCue_diagnostics_fallback.log"));

    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => Box::new(file),
        Err(error) => {
            eprintln!(
                "Could not append to diagnostic log '{}': {error}",
                path.display()
            );

            // If a removable/network folder disappears during a test session,
            // redirect both log streams to the standard writable location.
            let fallback = default_log_directory();
            if path.parent() != Some(fallback.as_path())
                && fs::create_dir_all(&fallback).is_ok()
                && set_log_directory(&fallback).is_ok()
            {
                let fallback_path = paths.read().ok().map(|paths| {
                    if audio {
                        paths.audio.clone()
                    } else {
                        paths.application.clone()
                    }
                });
                if let Some(fallback_path) = fallback_path {
                    if let Ok(mut file) = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(fallback_path)
                    {
                        let _ = writeln!(
                            file,
                            "[WARN] Previous diagnostic folder became unavailable; logging redirected to '{}'.",
                            fallback.display()
                        );
                        return Box::new(file);
                    }
                }
            }

            Box::new(io::sink())
        }
    }
}

/// Change the destination for the remainder of the current session. The
/// existing files are not moved; new events are written to the new folder.
pub fn set_log_directory(directory: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(directory)?;
    let Some(shared_paths) = LOG_PATHS.get() else {
        anyhow::bail!("Diagnostic logging has not been initialized");
    };

    let current = shared_paths
        .read()
        .map_err(|_| anyhow::anyhow!("Diagnostic log path lock is poisoned"))?
        .clone();

    let audio_name = current
        .audio
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "audio.log".into());
    let application_name = current
        .application
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "application.log".into());
    let report_name = current
        .report
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "diagnostic_report.txt".into());

    let next = LogPaths {
        directory: directory.to_path_buf(),
        audio: directory.join(audio_name),
        application: directory.join(application_name),
        report: directory.join(report_name),
        latest: directory.join("LATEST.txt"),
    };

    // Validate the destination before switching the live writers. If it is
    // unavailable, the previous folder remains active.
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&next.audio)?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&next.application)?;
    write_basic_report(&next.report, directory)?;
    prune_log_files(directory)?;
    write_latest_file(&next)?;

    *shared_paths
        .write()
        .map_err(|_| anyhow::anyhow!("Diagnostic log path lock is poisoned"))? = next;

    Ok(())
}

/// Write runtime details after the audio engine has initialized.
pub fn write_runtime_report(
    backend: &str,
    output_devices: &[String],
    input_devices: &[String],
    output_channels: usize,
) -> io::Result<()> {
    let Some(shared_paths) = LOG_PATHS.get() else {
        return Ok(());
    };
    let paths = shared_paths
        .read()
        .map_err(|_| io::Error::other("Diagnostic log path lock is poisoned"))?
        .clone();

    let mut report = String::new();
    report.push_str("Theatre Show Cue diagnostic report\n");
    report.push_str(&format!("Generated: {}\n", Local::now().to_rfc3339()));
    report.push_str(&format!(
        "Application version: {}\n",
        env!("CARGO_PKG_VERSION")
    ));
    report.push_str(&format!("Operating system: {}\n", std::env::consts::OS));
    report.push_str(&format!("Architecture: {}\n", std::env::consts::ARCH));
    report.push_str(&format!(
        "Build profile: {}\n",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    ));
    report.push_str(&format!(
        "Logical CPU cores: {}\n",
        std::thread::available_parallelism()
            .map(|count| count.get().to_string())
            .unwrap_or_else(|_| "unknown".to_string())
    ));
    report.push_str(&format!("Audio backend: {backend}\n"));
    report.push_str(&format!("Active output channels: {output_channels}\n"));
    report.push_str(&format!("Output device count: {}\n", output_devices.len()));
    for device in output_devices {
        report.push_str(&format!("  - {device}\n"));
    }
    report.push_str(&format!("Input device count: {}\n", input_devices.len()));
    for device in input_devices {
        report.push_str(&format!("  - {device}\n"));
    }
    report.push_str(&format!("Log directory: {}\n", paths.directory.display()));

    fs::write(paths.report, report)
}

fn write_basic_report(path: &Path, directory: &Path) -> io::Result<()> {
    let report = format!(
        "Theatre Show Cue diagnostic report\nGenerated: {}\nApplication version: {}\nOperating system: {}\nArchitecture: {}\nBuild profile: {}\nLogical CPU cores: {}\nAudio backend and device details are added after the audio engine initializes.\nLog directory: {}\n",
        Local::now().to_rfc3339(),
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        if cfg!(debug_assertions) { "debug" } else { "release" },
        std::thread::available_parallelism()
            .map(|count| count.get().to_string())
            .unwrap_or_else(|_| "unknown".to_string()),
        directory.display()
    );
    fs::write(path, report)
}

fn write_latest_file(paths: &LogPaths) -> io::Result<()> {
    let content = format!(
        "Latest Theatre Show Cue diagnostic session\nAudio: {}\nApplication: {}\nReport: {}\n",
        paths
            .audio
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("audio.log"),
        paths
            .application
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("application.log"),
        paths
            .report
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("diagnostic_report.txt")
    );
    fs::write(&paths.latest, content)
}

/// Retain no more than 30 files for each diagnostic file family. Only files
/// with our exact prefixes and extensions are considered.
fn prune_log_files(directory: &Path) -> io::Result<()> {
    for (prefix, extension) in [
        ("audio_", "log"),
        ("application_", "log"),
        ("diagnostic_report_", "txt"),
    ] {
        let mut files = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with(prefix)
                || path.extension().and_then(|value| value.to_str()) != Some(extension)
            {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            files.push((modified, path));
        }

        files.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.file_name().cmp(&right.1.file_name()))
        });
        let excess = files.len().saturating_sub(MAX_LOGS_PER_TYPE);
        for (_, path) in files.into_iter().take(excess) {
            if let Err(error) = fs::remove_file(&path) {
                if error.kind() != io::ErrorKind::NotFound {
                    return Err(error);
                }
            }
        }
    }
    Ok(())
}

/// Count the retained diagnostic files in a folder: audio, application and reports.
pub fn diagnostic_file_counts(directory: &Path) -> io::Result<(usize, usize, usize)> {
    let mut audio = 0usize;
    let mut application = 0usize;
    let mut reports = 0usize;

    for entry in fs::read_dir(directory)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("audio_")
            && entry.path().extension().and_then(|value| value.to_str()) == Some("log")
        {
            audio += 1;
        } else if name.starts_with("application_")
            && entry.path().extension().and_then(|value| value.to_str()) == Some("log")
        {
            application += 1;
        } else if name.starts_with("diagnostic_report_")
            && entry.path().extension().and_then(|value| value.to_str()) == Some("txt")
        {
            reports += 1;
        }
    }

    Ok((audio, application, reports))
}

#[derive(Debug)]
struct StoredZipEntry {
    name: Vec<u8>,
    crc32: u32,
    size: u32,
    local_header_offset: u32,
}

fn zip_crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn write_zip_u16(writer: &mut impl Write, value: u16) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_zip_u32(writer: &mut impl Write, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn is_diagnostic_archive_member(name: &str) -> bool {
    name == "LATEST.txt"
        || (name.starts_with("audio_") && name.ends_with(".log"))
        || (name.starts_with("application_") && name.ends_with(".log"))
        || (name.starts_with("diagnostic_report_") && name.ends_with(".txt"))
}

/// Export only Theatre Show Cue's own log files and LATEST.txt into a standard
/// ZIP archive. Uses ZIP's Store method so no additional compression crate is
/// required and the text logs remain directly inspectable.
pub fn create_diagnostic_archive(directory: &Path, destination: &Path) -> anyhow::Result<usize> {
    let mut source_files = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name_text) = name.to_str() else {
            continue;
        };
        if is_diagnostic_archive_member(name_text) {
            source_files.push((name_text.to_string(), entry.path()));
        }
    }
    source_files.sort_by(|left, right| left.0.cmp(&right.0));

    if source_files.is_empty() {
        anyhow::bail!(
            "No Theatre Show Cue diagnostic files were found in '{}'",
            directory.display()
        );
    }
    if let Some(name) = destination.file_name().and_then(|value| value.to_str()) {
        if is_diagnostic_archive_member(name) {
            anyhow::bail!("Choose a .zip filename that does not match a diagnostic log filename");
        }
    }
    if source_files.len() > u16::MAX as usize {
        anyhow::bail!("Too many diagnostic files to fit in a standard ZIP archive");
    }

    if let Some(parent) = destination.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut output = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(destination)?;

    let mut entries = Vec::with_capacity(source_files.len());
    const UTF8_FLAG: u16 = 0x0800;
    const DOS_DATE_1980_01_01: u16 = 0x0021;

    for (name, source_path) in source_files {
        let name_bytes = name.as_bytes();
        if name_bytes.len() > u16::MAX as usize {
            anyhow::bail!("Diagnostic filename is too long for ZIP: {name}");
        }
        let data = fs::read(&source_path)?;
        let size = u32::try_from(data.len()).map_err(|_| {
            anyhow::anyhow!(
                "Diagnostic file is larger than 4 GiB: {}",
                source_path.display()
            )
        })?;
        let local_header_offset = u32::try_from(output.stream_position()?).map_err(|_| {
            anyhow::anyhow!("Diagnostic archive exceeds the standard ZIP size limit")
        })?;
        let crc32 = zip_crc32(&data);

        // Local file header (compression method 0 = Store).
        write_zip_u32(&mut output, 0x0403_4b50)?;
        write_zip_u16(&mut output, 20)?;
        write_zip_u16(&mut output, UTF8_FLAG)?;
        write_zip_u16(&mut output, 0)?;
        write_zip_u16(&mut output, 0)?;
        write_zip_u16(&mut output, DOS_DATE_1980_01_01)?;
        write_zip_u32(&mut output, crc32)?;
        write_zip_u32(&mut output, size)?;
        write_zip_u32(&mut output, size)?;
        write_zip_u16(&mut output, name_bytes.len() as u16)?;
        write_zip_u16(&mut output, 0)?;
        output.write_all(name_bytes)?;
        output.write_all(&data)?;

        entries.push(StoredZipEntry {
            name: name_bytes.to_vec(),
            crc32,
            size,
            local_header_offset,
        });
    }

    let central_directory_offset = u32::try_from(output.stream_position()?)
        .map_err(|_| anyhow::anyhow!("Diagnostic archive exceeds the standard ZIP size limit"))?;

    for entry in &entries {
        // Central directory file header.
        write_zip_u32(&mut output, 0x0201_4b50)?;
        write_zip_u16(&mut output, 20)?;
        write_zip_u16(&mut output, 20)?;
        write_zip_u16(&mut output, UTF8_FLAG)?;
        write_zip_u16(&mut output, 0)?;
        write_zip_u16(&mut output, 0)?;
        write_zip_u16(&mut output, DOS_DATE_1980_01_01)?;
        write_zip_u32(&mut output, entry.crc32)?;
        write_zip_u32(&mut output, entry.size)?;
        write_zip_u32(&mut output, entry.size)?;
        write_zip_u16(&mut output, entry.name.len() as u16)?;
        write_zip_u16(&mut output, 0)?;
        write_zip_u16(&mut output, 0)?;
        write_zip_u16(&mut output, 0)?;
        write_zip_u16(&mut output, 0)?;
        write_zip_u32(&mut output, 0)?;
        write_zip_u32(&mut output, entry.local_header_offset)?;
        output.write_all(&entry.name)?;
    }

    let central_directory_size = u32::try_from(output.stream_position()?)
        .map_err(|_| anyhow::anyhow!("Diagnostic archive exceeds the standard ZIP size limit"))?
        .checked_sub(central_directory_offset)
        .ok_or_else(|| anyhow::anyhow!("Invalid ZIP central directory offset"))?;

    // End of central directory record.
    write_zip_u32(&mut output, 0x0605_4b50)?;
    write_zip_u16(&mut output, 0)?;
    write_zip_u16(&mut output, 0)?;
    write_zip_u16(&mut output, entries.len() as u16)?;
    write_zip_u16(&mut output, entries.len() as u16)?;
    write_zip_u32(&mut output, central_directory_size)?;
    write_zip_u32(&mut output, central_directory_offset)?;
    write_zip_u16(&mut output, 0)?;
    output.flush()?;

    Ok(entries.len())
}

/// Open the folder containing the current session's diagnostic files.
pub fn open_log_directory(directory: &Path) -> io::Result<()> {
    fs::create_dir_all(directory)?;

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer.exe")
            .arg(directory)
            .spawn()
            .map(|_| ())
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(directory)
            .spawn()
            .map(|_| ())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(directory)
            .spawn()
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::prune_log_files;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn crc32_matches_standard_check_value() {
        assert_eq!(super::zip_crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn diagnostic_archive_contains_only_owned_files() {
        let directory = std::env::temp_dir().join(format!("tasq-log-archive-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("audio_test.log"), "audio").unwrap();
        fs::write(directory.join("application_test.log"), "application").unwrap();
        fs::write(directory.join("diagnostic_report_test.txt"), "report").unwrap();
        fs::write(directory.join("LATEST.txt"), "latest").unwrap();
        fs::write(directory.join("unrelated.txt"), "do not include").unwrap();

        let archive_path = directory.join("diagnostics.zip");
        let count = super::create_diagnostic_archive(&directory, &archive_path).unwrap();
        let archive = fs::read(&archive_path).unwrap();

        assert_eq!(count, 4);
        assert_eq!(&archive[..4], b"PK\x03\x04");
        assert!(archive.windows(4).any(|window| window == b"PK\x01\x02"));
        assert!(archive.windows(4).any(|window| window == b"PK\x05\x06"));
        assert!(!archive
            .windows(14)
            .any(|window| window == b"do not include"));

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn pruning_keeps_thirty_files_of_each_type_and_ignores_other_files() {
        let directory = std::env::temp_dir().join(format!("tasq-log-prune-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();

        for index in 0..35 {
            fs::write(directory.join(format!("audio_{index:03}.log")), "audio").unwrap();
            fs::write(
                directory.join(format!("application_{index:03}.log")),
                "application",
            )
            .unwrap();
            fs::write(
                directory.join(format!("diagnostic_report_{index:03}.txt")),
                "report",
            )
            .unwrap();
        }
        fs::write(directory.join("LATEST.txt"), "latest").unwrap();
        fs::write(directory.join("keep_me.txt"), "keep").unwrap();

        prune_log_files(&directory).unwrap();

        for prefix in ["audio_", "application_", "diagnostic_report_"] {
            let count = fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
                .count();
            assert_eq!(count, 30, "{prefix}");
        }
        assert!(directory.join("LATEST.txt").exists());
        assert!(directory.join("keep_me.txt").exists());

        let _ = fs::remove_dir_all(&directory);
    }
}
