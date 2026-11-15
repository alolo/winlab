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

/// Project-scoped logical audio configuration. Keys use the same
/// `backend::device-name` identity as the audio engine so a project can retain
/// a distinct mapping and default stereo pair for every endpoint it uses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectAudioRouting {
    /// Logical TaSQ output slot -> native driver channel index, per endpoint.
    pub output_channel_orders: std::collections::HashMap<String, Vec<usize>>,
    /// Default stereo left/right logical TaSQ output slots, per endpoint.
    pub default_output_pairs: std::collections::HashMap<String, [usize; 2]>,
}

impl ProjectAudioRouting {
    pub fn from_maps(
        output_channel_orders: std::collections::HashMap<String, Vec<usize>>,
        default_output_pairs: std::collections::HashMap<String, [usize; 2]>,
    ) -> Self {
        Self {
            output_channel_orders,
            default_output_pairs,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub version: u32,
    pub settings: ProjectSettings,
    pub cue_list: CueList,
    /// None means this is a legacy project created before audio routing was
    /// stored per project. The app migrates it from workstation defaults once.
    #[serde(default)]
    pub audio_routing: Option<ProjectAudioRouting>,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            // Version 4 stores both per-Cue logical routes and the project's
            // endpoint-specific Logical Output Mapping / Default Stereo Pair.
            version: 4,
            settings: ProjectSettings::default(),
            cue_list: CueList::new("Main"),
            audio_routing: Some(ProjectAudioRouting::default()),
        }
    }
}

impl Project {
    pub fn with_audio_routing(audio_routing: ProjectAudioRouting) -> Self {
        Self {
            audio_routing: Some(audio_routing),
            ..Self::default()
        }
    }

    /// Save media paths relative to the project file whenever the media is
    /// inside the project directory. External media references remain absolute.
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut project = self.clone();
        project.version = project.version.max(4);
        project
            .audio_routing
            .get_or_insert_with(ProjectAudioRouting::default);

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
        project.version = project.version.max(4);
        project
            .audio_routing
            .get_or_insert_with(ProjectAudioRouting::default);
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

#[cfg(test)]
mod project_version_tests {
    use super::{Project, ProjectAudioRouting};
    use crate::config::AudioBackend;
    use crate::cue::{ChannelRoute, Cue};
    use std::path::PathBuf;
    use uuid::Uuid;

    #[test]
    fn cue_endpoint_and_logical_routes_round_trip_in_project_file() {
        let path =
            std::env::temp_dir().join(format!("tasq-routing-roundtrip-{}.tsq", Uuid::new_v4()));
        let mut project = Project::with_audio_routing(ProjectAudioRouting::from_maps(
            [(
                "WASAPI::Creative Sound Blaster Omni".to_string(),
                vec![2, 3, 0, 1],
            )]
            .into_iter()
            .collect(),
            [("WASAPI::Creative Sound Blaster Omni".to_string(), [2, 3])]
                .into_iter()
                .collect(),
        ));
        let mut cue = Cue::new_audio(PathBuf::from("audio/test.wav"));
        cue.io.output_device = Some("Creative Sound Blaster Omni".to_string());
        cue.io.output_device_id = Some("wasapi:{creative-endpoint}".to_string());
        cue.io.output_backend = Some(AudioBackend::Wasapi);
        cue.io.channel_routes = vec![
            ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: 2,
                output_channels: vec![2],
                gain_db: -3.0,
            },
            ChannelRoute {
                active: true,
                master_enabled: true,
                output_channel: 3,
                output_channels: vec![3],
                gain_db: -1.5,
            },
        ];
        cue.io.output_gains_db = vec![0.0, -1.0, -2.0, -3.0];
        project.cue_list.add(cue);

        project.save(&path).expect("project should save");
        let loaded = Project::load(&path).expect("project should load");
        let loaded_cue = loaded.cue_list.cues.first().expect("cue should persist");

        assert_eq!(
            loaded_cue.io.output_device.as_deref(),
            Some("Creative Sound Blaster Omni")
        );
        assert_eq!(
            loaded_cue.io.output_device_id.as_deref(),
            Some("wasapi:{creative-endpoint}")
        );
        assert_eq!(loaded_cue.io.output_backend, Some(AudioBackend::Wasapi));
        assert_eq!(loaded_cue.io.channel_routes[0].output_channels, vec![2]);
        assert_eq!(loaded_cue.io.channel_routes[1].output_channels, vec![3]);
        assert_eq!(loaded_cue.io.output_gains_db, vec![0.0, -1.0, -2.0, -3.0]);
        let audio_routing = loaded
            .audio_routing
            .as_ref()
            .expect("project audio routing should persist");
        assert_eq!(
            audio_routing.output_channel_orders["WASAPI::Creative Sound Blaster Omni"],
            vec![2, 3, 0, 1]
        );
        assert_eq!(
            audio_routing.default_output_pairs["WASAPI::Creative Sound Blaster Omni"],
            [2, 3]
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn project_backup_preserves_project_scoped_audio_routing() {
        let temp_dir =
            std::env::temp_dir().join(format!("tasq-audio-routing-backup-{}", Uuid::new_v4()));
        let project_path = temp_dir.join("Show.tsq");
        let backup_dir = temp_dir.join("backup");
        let backup_path = backup_dir.join("Show__snapshot.tsq");
        std::fs::create_dir_all(&temp_dir).expect("temp project directory should be created");

        let project = Project::with_audio_routing(ProjectAudioRouting::from_maps(
            [("ASIO::TASCAM".to_string(), vec![4, 5, 0, 1, 2, 3])]
                .into_iter()
                .collect(),
            [("ASIO::TASCAM".to_string(), [2, 3])].into_iter().collect(),
        ));
        project
            .save_backup(&backup_path, &project_path)
            .expect("project backup should save");

        let loaded = Project::load(&backup_path).expect("project backup should load");
        let routing = loaded
            .audio_routing
            .as_ref()
            .expect("backup should include project audio routing");
        assert_eq!(
            routing.output_channel_orders["ASIO::TASCAM"],
            vec![4, 5, 0, 1, 2, 3]
        );
        assert_eq!(routing.default_output_pairs["ASIO::TASCAM"], [2, 3]);
        assert_eq!(loaded.version, 4);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn new_projects_use_version_four_and_include_project_audio_routing() {
        let project = Project::default();
        assert_eq!(project.version, 4);
        assert!(project.audio_routing.is_some());
    }

    #[test]
    fn version_one_project_files_remain_deserializable_for_migration() {
        let json = r#"{
            "version": 1,
            "settings": {
                "name": "Legacy",
                "author": "",
                "osc_listen_port": 8000,
                "osc_send_addr": "127.0.0.1:9000",
                "audio_device": "Default",
                "auto_crossfade": false,
                "crossfade_secs": 2.0
            },
            "cue_list": {"name": "Main", "cues": []}
        }"#;
        let project: Project =
            serde_json::from_str(json).expect("version-one projects should still load");
        assert_eq!(project.version, 1);
        assert!(project.audio_routing.is_none());
    }
}
