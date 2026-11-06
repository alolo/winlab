// src/export/score_export.rs — генерация текста партитуры по шаблону

use crate::cue::CueList;
use anyhow::Result;
use std::path::Path;

#[allow(dead_code)]
pub const DEFAULT_TEMPLATE: &str = "{number}\t{name}\t{duration}\t{notes}";

/// Шаблонные переменные:
/// {number}, {name}, {file}, {duration}, {notes}, {fade_in}, {fade_out}
#[allow(dead_code)]
pub fn export_score(cue_list: &CueList, template: &str, output_path: &Path) -> Result<()> {
    let mut lines = Vec::new();

    for cue in &cue_list.cues {
        let duration = format_duration(cue.effective_duration());
        let line = template
            .replace("{number}", &cue.number)
            .replace("{name}", &cue.name)
            .replace(
                "{file}",
                &cue.file_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(""),
            )
            .replace("{duration}", &duration)
            .replace("{notes}", &cue.notes)
            .replace("{fade_in}", &format!("{:.1}", cue.fade_in_secs))
            .replace("{fade_out}", &format!("{:.1}", cue.fade_out_secs));
        lines.push(line);
    }

    let content = lines.join("\n");
    std::fs::write(output_path, content)?;
    Ok(())
}

#[allow(dead_code)]
fn format_duration(secs: f64) -> String {
    let m = (secs / 60.0) as u32;
    let s = secs as u32 % 60;
    let ms = ((secs.fract()) * 1000.0) as u32;
    format!("{m:02}:{s:02}.{ms:03}")
}
