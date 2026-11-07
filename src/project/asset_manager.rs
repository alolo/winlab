// src/project/asset_manager.rs — менеджер копирования аудио в папку audio проекта

use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing::info;

pub struct AssetManager {
    pub project_path: Option<PathBuf>,
}

impl AssetManager {
    pub fn new() -> Self {
        Self { project_path: None }
    }

    pub fn set_project_path(&mut self, path: PathBuf) {
        self.project_path = Some(path);
    }

    pub fn import_file(&self, src: &Path) -> Result<PathBuf> {
        // An unsaved project has no audio directory yet. Keep the original file
        // reference for now; save_project() will migrate it after the project is named.
        let Some(project_path) = self.project_path.as_ref() else {
            return Ok(src.to_path_buf());
        };

        let audio_dir = project_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("audio");
        std::fs::create_dir_all(&audio_dir)?;

        let file_name = src
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("Invalid file name"))?;
        let mut destination = audio_dir.join(file_name);

        if destination.as_path() == src || Self::same_file(src, &destination) {
            return Ok(destination);
        }

        // Avoid silently reusing an unrelated asset with the same filename.
        if destination.exists() {
            let stem = src
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("audio");
            let extension = src.extension().and_then(|value| value.to_str());
            let mut suffix = 2_u32;
            loop {
                let name = match extension {
                    Some(extension) => format!("{stem}_{suffix}.{extension}"),
                    None => format!("{stem}_{suffix}"),
                };
                destination = audio_dir.join(name);
                if !destination.exists() {
                    break;
                }
                suffix += 1;
            }
        }

        std::fs::copy(src, &destination)?;
        info!("Asset imported: {:?} → {:?}", src, destination);
        Ok(destination)
    }

    fn same_file(left: &Path, right: &Path) -> bool {
        match (left.canonicalize(), right.canonicalize()) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
    }

    #[allow(dead_code)]
    pub fn make_relative(&self, abs: &Path) -> PathBuf {
        if let Some(proj) = &self.project_path {
            if let Some(proj_dir) = proj.parent() {
                if let Ok(rel) = abs.strip_prefix(proj_dir) {
                    return rel.to_path_buf();
                }
            }
        }
        abs.to_path_buf()
    }

    #[allow(dead_code)]
    pub fn resolve(&self, rel: &Path) -> PathBuf {
        if rel.is_absolute() {
            return rel.to_path_buf();
        }
        if let Some(proj) = &self.project_path {
            if let Some(proj_dir) = proj.parent() {
                return proj_dir.join(rel);
            }
        }
        rel.to_path_buf()
    }
}
