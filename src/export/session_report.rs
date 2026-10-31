// src/export/session_report.rs — лог сессии воспроизведения
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::info;
use anyhow::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayEvent {
    pub timestamp: DateTime<Local>,
    pub cue_number: String,
    pub cue_name: String,
    pub duration_played_secs: f64,
    pub action: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SessionReport {
    pub session_start: Option<DateTime<Local>>,
    pub session_end: Option<DateTime<Local>>,
    pub project_name: String,
    pub events: Vec<PlayEvent>,
}

impl SessionReport {
    pub fn new(project_name: &str) -> Self {
        Self {
            session_start: Some(Local::now()),
            session_end: None,
            project_name: project_name.to_string(),
            events: Vec::new(),
        }
    }

    pub fn record(&mut self, cue_number: &str, cue_name: &str, duration: f64, action: &str) {
        self.events.push(PlayEvent {
            timestamp: Local::now(),
            cue_number: cue_number.to_string(),
            cue_name: cue_name.to_string(),
            duration_played_secs: duration,
            action: action.to_string(),
        });
    }

    pub fn finish(&mut self) {
        self.session_end = Some(Local::now());
    }

    pub fn save(&self, reports_dir: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(reports_dir)?;
        let ts = Local::now().format("%Y-%m-%d_%H-%M-%S");
        let fname = format!("session_{ts}.json");
        let path = reports_dir.join(&fname);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        info!("Session report saved: {:?}", path);
        Ok(path)
    }

    pub fn to_text_summary(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("=== Session Report: {} ===\n", self.project_name));
        if let Some(start) = self.session_start {
            out.push_str(&format!("Start: {}\n", start.format("%Y-%m-%d %H:%M:%S")));
        }
        if let Some(end) = self.session_end {
            out.push_str(&format!("End:   {}\n", end.format("%Y-%m-%d %H:%M:%S")));
        }
        out.push_str(&format!("Events: {}\n\n", self.events.len()));
        for e in &self.events {
            out.push_str(&format!(
                "[{}] {} — {} {} ({:.1}s)\n",
                e.timestamp.format("%H:%M:%S"),
                e.cue_number,
                e.cue_name,
                e.action,
                e.duration_played_secs
            ));
        }
        out
    }
}