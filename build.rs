use std::env;
use std::path::PathBuf;
use std::process::Command;

fn find_resource_compiler() -> Option<PathBuf> {
    if let Some(path) = env::var_os("RC") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }

    // Prefer the compiler already available through the active Visual Studio
    // / Windows SDK developer environment.
    if Command::new("rc.exe").arg("/?").output().is_ok() {
        return Some(PathBuf::from("rc.exe"));
    }

    // A regular PowerShell session may not have the Windows SDK bin folder on
    // PATH even though the SDK is installed. Probe the standard installation.
    let program_files_x86 = env::var_os("ProgramFiles(x86)")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files (x86)"));
    let sdk_bin = program_files_x86.join(r"Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(sdk_bin)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    versions.sort();
    versions.reverse();

    for version in versions {
        for architecture in ["x64", "x86"] {
            let candidate = version.join(architecture).join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}

fn main() {
    println!("cargo:rerun-if-changed=assets/TaSQ.png");
    println!("cargo:rerun-if-changed=assets/TaSQ_multi.ico");
    println!("cargo:rerun-if-changed=assets/TaSQ.rc");
    for icon in [
        "audio.svg",
        "comment.svg",
        "fadeIn.svg",
        "fadeOut.svg",
        "group.svg",
        "midi.svg",
        "notes.svg",
        "osc.svg",
        "pause.svg",
        "play.svg",
        "settings.svg",
        "stop.svg",
    ] {
        println!("cargo:rerun-if-changed=assets/icons/{icon}");
    }
    println!("cargo:rerun-if-env-changed=RC");

    if env::var("CARGO_CFG_TARGET_OS").ok().as_deref() != Some("windows") {
        return;
    }

    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let resource_file = out_dir.join("TaSQ.res");
    let compiler = find_resource_compiler().unwrap_or_else(|| {
        panic!(
            "Could not find rc.exe. Install the Windows SDK or run Cargo from a Visual Studio Developer PowerShell."
        )
    });

    let status = Command::new(&compiler)
        .current_dir(&manifest_dir)
        .arg("/nologo")
        .arg(format!("/fo{}", resource_file.display()))
        .arg("assets/TaSQ.rc")
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "Could not run Windows resource compiler '{}': {error}",
                compiler.display()
            )
        });

    if !status.success() {
        panic!(
            "Windows resource compiler '{}' failed with status {status}",
            compiler.display()
        );
    }

    // The .res input is linked into the TaSQ executable as its native Windows
    // application icon (Explorer, taskbar and Alt+Tab).
    println!("cargo:rustc-link-arg-bin=TaSQ={}", resource_file.display());
}
