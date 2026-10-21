// Asset manager — copies audio files into project folder on save

use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

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

    /// Copy a file into project assets dir, return new path
    pub fn import_file(&self, src: &Path) -> Result<PathBuf> {
        let project_path = self
            .project_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No project path set — save project first"))?;

        let assets_dir = project_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("assets");

        std::fs::create_dir_all(&assets_dir)?;

        let file_name = src
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("Invalid file name"))?;

        let dst = assets_dir.join(file_name);

        if dst == src {
            return Ok(dst); // already in project folder
        }

        if dst.exists() {
            warn!("Asset {:?} already exists, skipping copy", dst);
            return Ok(dst);
        }

        std::fs::copy(src, &dst)?;
        info!("Asset imported: {:?} → {:?}", src, dst);
        Ok(dst)
    }

    /// Make path relative to project directory
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

    /// Resolve relative path back to absolute
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
