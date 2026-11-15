use crate::diagnostics;
use crate::ui::theme::Theme;
use egui::{self, RichText, Ui};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const MAX_VISIBLE_LOG_LINES: usize = 5_000;
const MAX_INITIAL_READ_BYTES: u64 = 2 * 1024 * 1024;
const REFRESH_INTERVAL: Duration = Duration::from_millis(250);

/// Live tail of the current audio diagnostic log.
pub struct AudioLogViewer {
    path: Option<PathBuf>,
    offset: u64,
    pending_bytes: Vec<u8>,
    skip_first_partial_line: bool,
    lines: VecDeque<String>,
    last_refresh: Option<Instant>,
    last_error: Option<String>,
    show_all: bool,
    show_warn: bool,
    show_error: bool,
}

impl Default for AudioLogViewer {
    fn default() -> Self {
        Self {
            path: None,
            offset: 0,
            pending_bytes: Vec::new(),
            skip_first_partial_line: false,
            lines: VecDeque::new(),
            last_refresh: None,
            last_error: None,
            show_all: true,
            show_warn: false,
            show_error: false,
        }
    }
}

impl AudioLogViewer {
    pub fn show(&mut self, ui: &mut Ui) {
        let now = Instant::now();
        let refresh_due = self
            .last_refresh
            .map(|last| now.saturating_duration_since(last) >= REFRESH_INTERVAL)
            .unwrap_or(true);

        if refresh_due {
            self.refresh();
            self.last_refresh = Some(now);
        }
        ui.ctx().request_repaint_after(REFRESH_INTERVAL);

        ui.horizontal_wrapped(|ui| {
            let all_changed = ui.checkbox(&mut self.show_all, "All information").changed();
            if all_changed && self.show_all {
                self.show_warn = false;
                self.show_error = false;
            }

            let warn_changed = ui.checkbox(&mut self.show_warn, "WARN").changed();
            if warn_changed && self.show_warn {
                self.show_all = false;
            }

            let error_changed = ui.checkbox(&mut self.show_error, "ERROR").changed();
            if error_changed && self.show_error {
                self.show_all = false;
            }
        });
        self.normalize_filter_selection();

        ui.horizontal(|ui| {
            let file_name = self
                .path
                .as_deref()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "audio log unavailable".to_string());
            ui.label(
                RichText::new(format!("{file_name} · live"))
                    .size(Theme::scaled_font_size(10.5))
                    .color(Theme::text_dim()),
            );
            ui.separator();
            ui.label(
                RichText::new(format!("{} lines in buffer", self.lines.len()))
                    .size(Theme::scaled_font_size(10.5))
                    .color(Theme::text_dim()),
            );
        });

        if let Some(error) = &self.last_error {
            ui.colored_label(
                egui::Color32::from_rgb(235, 85, 85),
                format!("Could not read audio log: {error}"),
            );
        }

        let filtered: Vec<&str> = self
            .lines
            .iter()
            .map(String::as_str)
            .filter(|line| self.matches_filter(line))
            .collect();

        let display_text = if !self.show_all && !self.show_warn && !self.show_error {
            String::new()
        } else {
            filtered.join("\n")
        };

        let scroll_height = ui.available_height().max(80.0);
        egui::ScrollArea::vertical()
            .id_salt("audio_log_viewer_scroll")
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .max_height(scroll_height)
            .show(ui, |ui| {
                if !self.show_all && !self.show_warn && !self.show_error {
                    ui.label(
                        RichText::new(
                            "Select All information, WARN or ERROR to view the audio log.",
                        )
                        .color(Theme::text_dim()),
                    );
                } else if display_text.is_empty() {
                    ui.label(
                        RichText::new("No matching audio log entries yet.")
                            .color(Theme::text_dim()),
                    );
                } else {
                    ui.add(
                        egui::Label::new(
                            RichText::new(display_text)
                                .monospace()
                                .size(Theme::scaled_font_size(10.5))
                                .color(Theme::text_dim()),
                        )
                        .selectable(true)
                        .wrap(),
                    );
                }
            });
    }

    fn normalize_filter_selection(&mut self) {
        if self.show_all {
            self.show_warn = false;
            self.show_error = false;
        } else if !self.show_warn && !self.show_error {
            // Keep one view active at all times. Clearing the last severity
            // filter returns the viewer to the complete audio log.
            self.show_all = true;
        }
    }

    fn matches_filter(&self, line: &str) -> bool {
        if self.show_all {
            return true;
        }

        match log_level(line) {
            Some("WARN") => self.show_warn,
            Some("ERROR") => self.show_error,
            _ => false,
        }
    }

    fn refresh(&mut self) {
        let Some(path) = diagnostics::active_audio_log_path() else {
            self.last_error = Some("diagnostic logging is not initialized".to_string());
            return;
        };

        let path_changed = self.path.as_ref() != Some(&path);
        if path_changed {
            self.path = Some(path.clone());
            self.offset = 0;
            self.pending_bytes.clear();
            self.skip_first_partial_line = false;
            self.lines.clear();
        }

        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) => {
                self.last_error = Some(error.to_string());
                return;
            }
        };

        let file_len = match file.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                self.last_error = Some(error.to_string());
                return;
            }
        };

        // Load only the tail when the viewer first opens; the complete log
        // remains on disk and is included in the diagnostic ZIP.
        if path_changed {
            self.offset = file_len.saturating_sub(MAX_INITIAL_READ_BYTES);
            self.skip_first_partial_line = self.offset > 0;
        }

        // A truncated/recreated log starts a new stream.
        if file_len < self.offset {
            self.offset = 0;
            self.pending_bytes.clear();
            self.skip_first_partial_line = false;
            self.lines.clear();
        }

        if let Err(error) = file.seek(SeekFrom::Start(self.offset)) {
            self.last_error = Some(error.to_string());
            return;
        }

        let mut new_bytes = Vec::new();
        if let Err(error) = file.read_to_end(&mut new_bytes) {
            self.last_error = Some(error.to_string());
            return;
        }
        self.offset = self.offset.saturating_add(new_bytes.len() as u64);
        self.pending_bytes.extend_from_slice(&new_bytes);

        if self.skip_first_partial_line {
            if let Some(first_newline) = self.pending_bytes.iter().position(|byte| *byte == b'\n') {
                self.pending_bytes.drain(..=first_newline);
                self.skip_first_partial_line = false;
            } else {
                self.last_error = None;
                return;
            }
        }

        // Keep an incomplete trailing line until its newline arrives. This also
        // avoids decoding a UTF-8 character split across two file reads.
        if let Some(last_newline) = self.pending_bytes.iter().rposition(|byte| *byte == b'\n') {
            let complete_bytes: Vec<u8> = self.pending_bytes.drain(..=last_newline).collect();
            let complete_text = String::from_utf8_lossy(&complete_bytes);
            for line in complete_text.lines() {
                self.lines.push_back(strip_ansi_sequences(line));
                while self.lines.len() > MAX_VISIBLE_LOG_LINES {
                    self.lines.pop_front();
                }
            }
        }

        self.last_error = None;
    }
}

fn log_level(line: &str) -> Option<&str> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let outer_level = *fields.get(1)?;

    // Worker stdout/stderr is wrapped by the parent logger. Prefer the
    // child's original level when the line contains a nested child record.
    if let Some(target_index) = fields
        .iter()
        .position(|field| field.contains("TaSQ::audio::worker::child:"))
    {
        if let Some(child_level) = fields.get(target_index + 2).copied() {
            if is_log_level(child_level) {
                return Some(child_level);
            }
        }
    }

    is_log_level(outer_level).then_some(outer_level)
}

fn is_log_level(level: &str) -> bool {
    matches!(level, "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR")
}

fn strip_ansi_sequences(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut characters = input.chars();

    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            // CSI/SGR escape sequence: skip through its terminating letter.
            for escaped in characters.by_ref() {
                if escaped.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            output.push(character);
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::{log_level, strip_ansi_sequences, AudioLogViewer};

    #[test]
    fn clearing_last_severity_filter_restores_all_information() {
        let mut viewer = AudioLogViewer::default();
        viewer.show_all = false;
        viewer.show_warn = true;
        viewer.show_error = false;

        viewer.show_warn = false;
        viewer.normalize_filter_selection();

        assert!(viewer.show_all);
        assert!(!viewer.show_warn);
        assert!(!viewer.show_error);
        assert!(viewer.matches_filter("2026-11-15T01:00:00.000Z INFO TaSQ::audio: ready"));
    }

    #[test]
    fn clearing_one_of_two_severity_filters_keeps_the_other_active() {
        let mut viewer = AudioLogViewer::default();
        viewer.show_all = false;
        viewer.show_warn = true;
        viewer.show_error = true;

        viewer.show_warn = false;
        viewer.normalize_filter_selection();

        assert!(!viewer.show_all);
        assert!(!viewer.show_warn);
        assert!(viewer.show_error);
        assert!(viewer.matches_filter("2026-11-15T01:00:00.000Z ERROR TaSQ::audio: failed"));
        assert!(!viewer.matches_filter("2026-11-15T01:00:00.000Z INFO TaSQ::audio: ready"));
    }

    #[test]
    fn all_information_clears_severity_filters() {
        let mut viewer = AudioLogViewer::default();
        viewer.show_all = true;
        viewer.show_warn = true;
        viewer.show_error = true;

        viewer.normalize_filter_selection();

        assert!(viewer.show_all);
        assert!(!viewer.show_warn);
        assert!(!viewer.show_error);
    }

    #[test]
    fn warn_and_error_filters_can_be_combined() {
        let mut viewer = super::AudioLogViewer::default();
        viewer.show_all = false;
        viewer.show_warn = true;
        viewer.show_error = true;

        assert!(viewer.matches_filter("2026-11-15T01:00:00.000Z WARN TaSQ::audio: lost"));
        assert!(viewer.matches_filter("2026-11-15T01:00:00.000Z ERROR TaSQ::audio: failed"));
        assert!(!viewer.matches_filter("2026-11-15T01:00:00.000Z INFO TaSQ::audio: ready"));
    }

    #[test]
    fn parses_standard_tracing_levels() {
        assert_eq!(
            log_level("2026-11-15T01:00:00.000Z  INFO TaSQ::audio::engine: ready"),
            Some("INFO")
        );
        assert_eq!(
            log_level("2026-11-15T01:00:00.000Z  WARN TaSQ::audio::engine: lost"),
            Some("WARN")
        );
        assert_eq!(
            log_level("2026-11-15T01:00:00.000Z ERROR TaSQ::audio::engine: failed"),
            Some("ERROR")
        );
    }

    #[test]
    fn ignores_non_log_continuation_lines_for_level_filters() {
        assert_eq!(
            log_level("worker child output continued on this line"),
            None
        );
    }

    #[test]
    fn reads_original_level_from_wrapped_worker_output() {
        let line = "2026-11-15T01:00:00.000Z INFO TaSQ::audio::worker::child: \u{1b}[2m2026-11-15T01:00:00.000Z\u{1b}[0m \u{1b}[33m WARN\u{1b}[0m TaSQ::audio::engine: driver warning";
        let clean = strip_ansi_sequences(line);
        assert_eq!(log_level(&clean), Some("WARN"));
    }

    #[test]
    fn reads_original_error_level_from_wrapped_worker_output() {
        let line = "2026-11-15T01:00:00.000Z INFO TaSQ::audio::worker::child: \u{1b}[2m2026-11-15T01:00:00.000Z\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m TaSQ::audio::engine: stream failed";
        let clean = strip_ansi_sequences(line);
        assert_eq!(log_level(&clean), Some("ERROR"));
    }

    #[test]
    fn strips_ansi_sequences_from_display_text() {
        assert_eq!(
            strip_ansi_sequences("hello \u{1b}[31mERROR\u{1b}[0m world"),
            "hello ERROR world"
        );
    }
}
