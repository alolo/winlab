// Score export — generates text score from cue list using user template

use crate::cue::CueList;
use anyhow::Result;
use std::path::Path;

/// Template variables available:
/// {number}, {name}, {file}, {duration}, {notes}, {fade_in}, {fade_out}
pub fn export_score(cue_list: &CueList, template: &str, output_path: &Path) -> Result<()> {
    let mut lines = Vec::new();

    for cue in &cue_list.cues {
        let duration = format_duration(cue.duration_secs);
        let line = template
            .replace("{number}", &cue.number)
            .replace("{name}", &cue.name)
            .replace("{file}", &cue.file_path.file_name()
                .and_then(|n| n.to_str()).unwrap_or(""))
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

fn format_duration(secs: f64) -> String {
    let m = (secs / 60.0) as u32;
    let s = secs as u32 % 60;
    let ms = ((secs.fract()) * 1000.0) as u32;
    format!("{m:02}:{s:02}.{ms:03}")
}

pub const DEFAULT_TEMPLATE: &str = "{number}\t{name}\t{duration}\t{notes}";
