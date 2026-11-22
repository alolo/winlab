// src/project/asset_manager.rs — менеджер копирования аудио в папку audio проекта

use anyhow::Result;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use tracing::info;

pub struct AssetManager {
    pub project_path: Option<PathBuf>,
    /// Reuse the project copy when the same source file is imported more than once.
    imported_sources: HashMap<PathBuf, PathBuf>,
}

impl AssetManager {
    pub fn new() -> Self {
        Self {
            project_path: None,
            imported_sources: HashMap::new(),
        }
    }

    pub fn set_project_path(&mut self, path: PathBuf) {
        if self.project_path.as_deref() != Some(path.as_path()) {
            self.imported_sources.clear();
            self.project_path = Some(path);
        }
    }

    pub fn import_file(&mut self, src: &Path) -> Result<PathBuf> {
        // An unsaved project has no audio directory yet. Keep the original file
        // reference for now; save_project() will migrate it after the project is named.
        let Some(project_path) = self.project_path.as_ref() else {
            return Ok(src.to_path_buf());
        };

        let source_key = src.canonicalize().unwrap_or_else(|_| src.to_path_buf());
        if let Some(cached_destination) = self.imported_sources.get(&source_key) {
            if cached_destination.is_file() {
                return Ok(cached_destination.clone());
            }
        }

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
            self.imported_sources
                .insert(source_key, destination.clone());
            return Ok(destination);
        }

        // If the preferred destination already contains identical bytes, reuse it.
        // This also makes repeated imports safe after restarting the application,
        // when the in-memory source cache is empty.
        if destination.is_file() && Self::files_equal(src, &destination)? {
            self.imported_sources
                .insert(source_key, destination.clone());
            return Ok(destination);
        }

        // Avoid silently reusing an unrelated asset with the same filename.
        // Check existing suffixed variants for identical content before allocating
        // another name, so a repeated source never creates build-up_2, _3, etc.
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
                let candidate = audio_dir.join(name);
                if !candidate.exists() {
                    destination = candidate;
                    break;
                }
                if candidate.is_file() && Self::files_equal(src, &candidate)? {
                    destination = candidate;
                    self.imported_sources
                        .insert(source_key, destination.clone());
                    return Ok(destination);
                }
                suffix += 1;
            }
        }

        std::fs::copy(src, &destination)?;
        info!("Asset imported: {:?} → {:?}", src, destination);
        self.imported_sources
            .insert(source_key, destination.clone());
        Ok(destination)
    }

    fn same_file(left: &Path, right: &Path) -> bool {
        match (left.canonicalize(), right.canonicalize()) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
    }

    /// Compare files in bounded chunks rather than loading full audio assets into RAM.
    fn files_equal(left: &Path, right: &Path) -> Result<bool> {
        let left_metadata = std::fs::metadata(left)?;
        let right_metadata = std::fs::metadata(right)?;
        if left_metadata.len() != right_metadata.len() {
            return Ok(false);
        }

        let mut left_file = File::open(left)?;
        let mut right_file = File::open(right)?;
        let mut left_buffer = [0_u8; 64 * 1024];
        let mut right_buffer = [0_u8; 64 * 1024];

        loop {
            let left_read = left_file.read(&mut left_buffer)?;
            let right_read = right_file.read(&mut right_buffer)?;
            if left_read != right_read {
                return Ok(false);
            }
            if left_read == 0 {
                return Ok(true);
            }
            if left_buffer[..left_read] != right_buffer[..right_read] {
                return Ok(false);
            }
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

#[cfg(test)]
mod tests {
    use super::AssetManager;
    use std::fs;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tasq-assets-{label}-{}", Uuid::new_v4()))
    }

    #[test]
    fn repeated_import_of_same_source_reuses_one_project_asset() {
        let root = temp_root("same-source");
        let source = root.join("source").join("build-up.wav");
        let project = root.join("show.tsq");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"same audio bytes").unwrap();

        let mut manager = AssetManager::new();
        manager.set_project_path(project);
        let first = manager.import_file(&source).unwrap();
        let second = manager.import_file(&source).unwrap();

        assert_eq!(first, second);
        assert!(first.is_file());
        assert_eq!(
            fs::read_dir(root.join("audio")).unwrap().count(),
            1,
            "repeated Cue references must not create suffixed copies"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn different_content_with_same_filename_gets_a_distinct_asset() {
        let root = temp_root("different-content");
        let source_a = root.join("a").join("build-up.wav");
        let source_b = root.join("b").join("build-up.wav");
        let project = root.join("show.tsq");
        fs::create_dir_all(source_a.parent().unwrap()).unwrap();
        fs::create_dir_all(source_b.parent().unwrap()).unwrap();
        fs::write(&source_a, b"first audio").unwrap();
        fs::write(&source_b, b"second audio").unwrap();

        let mut manager = AssetManager::new();
        manager.set_project_path(project);
        let first = manager.import_file(&source_a).unwrap();
        let second = manager.import_file(&source_b).unwrap();

        assert_ne!(first, second);
        assert_eq!(fs::read(&first).unwrap(), b"first audio");
        assert_eq!(fs::read(&second).unwrap(), b"second audio");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn identical_existing_asset_is_reused_after_manager_restart() {
        let root = temp_root("existing-identical");
        let source = root.join("source").join("build-up.wav");
        let project = root.join("show.tsq");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"same bytes after restart").unwrap();

        let mut first_manager = AssetManager::new();
        first_manager.set_project_path(project.clone());
        let first = first_manager.import_file(&source).unwrap();

        let mut second_manager = AssetManager::new();
        second_manager.set_project_path(project);
        let second = second_manager.import_file(&source).unwrap();

        assert_eq!(first, second);
        assert_eq!(fs::read_dir(root.join("audio")).unwrap().count(), 1);
        let _ = fs::remove_dir_all(root);
    }
}
