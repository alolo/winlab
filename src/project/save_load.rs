// src/project/save_load.rs — сохранение и загрузка проектов .clabs

use crate::cue::CueList;
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
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        info!("Project saved to {:?}", path);
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let json = std::fs::read_to_string(path)?;
        let project: Project = serde_json::from_str(&json)?;
        info!("Project loaded from {:?}", path);
        Ok(project)
    }

    #[allow(dead_code)]
    pub fn project_dir(path: &Path) -> Option<PathBuf> {
        path.parent().map(|p| p.to_path_buf())
    }

    #[allow(dead_code)]
    pub fn assets_dir(project_path: &Path) -> PathBuf {
        let dir = project_path
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf();
        dir.join("assets")
    }
}