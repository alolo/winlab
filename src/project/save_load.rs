// src/project/save_load.rs — сохранение и загрузка проектов .tsq

use crate::cue::{CueList, CueType};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSettings {
    pub name: String,
    pub author: String,
    pub osc_listen_port: u16,
    pub osc_send_addr: String,
    pub audio_device: String,
    pub auto_crossfade: bool,
    pub crossfade_secs: f32,
}

impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            name: "Untitled Project".to_string(),
            author: String::new(),
            osc_listen_port: 8000,
            osc_send_addr: "127.0.0.1:9000".to_string(),
            audio_device: "Default".to_string(),
            auto_crossfade: false,
            crossfade_secs: 2.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub version: u32,
    pub settings: ProjectSettings,
    pub cue_list: CueList,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            version: 1,
            settings: ProjectSettings::default(),
            cue_list: CueList::new("Main"),
        }
    }
}

impl Project {
    /// Save media paths relative to the project file whenever the media is
    /// inside the project directory. External media references remain absolute.
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut project = self.clone();

        let is_backup = path
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("backup"));
        if !is_backup {
            if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
                if !stem.is_empty() {
                    project.settings.name = stem.to_string();
                }
            }
        }

        let project_dir = project_directory(path);
        for cue in &mut project.cue_list.cues {
            if !matches!(&cue.cue_type, CueType::Audio) || cue.file_path.as_os_str().is_empty() {
                continue;
            }

            if cue.file_path.is_absolute() {
                if let Ok(relative_path) = cue.file_path.strip_prefix(&project_dir) {
                    cue.file_path = relative_path.to_path_buf();
                }
            }
        }

        let json = serde_json::to_string_pretty(&project)?;
        std::fs::write(path, json)?;
        info!("Project saved to {:?}", path);
        Ok(())
    }

    /// Write a lightweight project snapshot into the `backup/` directory.
    /// Audio files are never copied. Media paths inside the project folder are
    /// stored relative to the backup directory (`../audio/...`) so that a
    /// backup can be opened directly without losing its media references.
    pub fn save_backup(&self, backup_path: &Path, source_project_path: &Path) -> Result<()> {
        let mut project = self.clone();
        let source_project_dir = project_directory(source_project_path);

        for cue in &mut project.cue_list.cues {
            if !matches!(&cue.cue_type, CueType::Audio) || cue.file_path.as_os_str().is_empty() {
                continue;
            }

            if cue.file_path.is_absolute() {
                if let Ok(relative_path) = cue.file_path.strip_prefix(&source_project_dir) {
                    cue.file_path = PathBuf::from("..").join(relative_path);
                }
            } else {
                // In-memory relative paths are interpreted from the original
                // project directory, not from the backup subdirectory.
                cue.file_path = PathBuf::from("..").join(&cue.file_path);
            }
        }

        if let Some(parent) = backup_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&project)?;
        std::fs::write(backup_path, json)?;
        info!("Project backup saved to {:?}", backup_path);
        Ok(())
    }

    /// Resolve relative media paths against the directory containing the
    /// project file. Absolute paths from older project files stay unchanged.
    pub fn load(path: &Path) -> Result<Self> {
        let json = std::fs::read_to_string(path)?;
        let mut project: Project = serde_json::from_str(&json)?;
        let project_dir = project_directory(path);
        let legacy_assets_dir = project_dir.join("assets");
        let audio_dir = project_dir.join("audio");

        // Migrate older projects that used `assets/` to the clearer `audio/`
        // folder. If both folders already exist, resolve each cue to whichever
        // location actually contains its file and keep legacy missing paths
        // working.
        let migrated_legacy_assets = legacy_assets_dir.is_dir()
            && !audio_dir.exists()
            && std::fs::rename(&legacy_assets_dir, &audio_dir).is_ok();
        if migrated_legacy_assets {
            info!(
                "Migrated project media folder: {:?} → {:?}",
                legacy_assets_dir, audio_dir
            );
        }

        for cue in &mut project.cue_list.cues {
            if !matches!(&cue.cue_type, CueType::Audio) || cue.file_path.as_os_str().is_empty() {
                continue;
            }

            if !cue.file_path.is_absolute() {
                if let Ok(path_inside_legacy_assets) = cue.file_path.strip_prefix("assets") {
                    let audio_candidate = audio_dir.join(path_inside_legacy_assets);
                    let legacy_candidate = project_dir.join(&cue.file_path);
                    cue.file_path = if migrated_legacy_assets
                        || audio_candidate.exists()
                        || (!legacy_candidate.exists() && audio_dir.exists())
                    {
                        audio_candidate
                    } else {
                        legacy_candidate
                    };
                } else {
                    cue.file_path = project_dir.join(&cue.file_path);
                }
            }
        }

        // Backup snapshots have timestamped filenames, but retain the
        // original project name stored in their JSON payload.
        let is_backup = path
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("backup"));
        if !is_backup {
            if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
                if !stem.is_empty() {
                    project.settings.name = stem.to_string();
                }
            }
        }

        info!("Project loaded from {:?}", path);
        Ok(project)
    }

    #[allow(dead_code)]
    pub fn project_dir(path: &Path) -> Option<PathBuf> {
        path.parent().map(|p| p.to_path_buf())
    }

    #[allow(dead_code)]
    pub fn audio_dir(project_path: &Path) -> PathBuf {
        project_directory(project_path).join("audio")
    }

    /// Backwards-compatible helper name; new projects use the `audio/` folder.
    #[allow(dead_code)]
    pub fn assets_dir(project_path: &Path) -> PathBuf {
        Self::audio_dir(project_path)
    }
}

fn project_directory(path: &Path) -> PathBuf {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current_dir| current_dir.join(parent))
            .unwrap_or_else(|_| parent.to_path_buf())
    }
}
