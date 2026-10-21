// App config — hotkeys and global settings

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AppAction {
    Go,
    StopSelected,
    StopAll,
    PauseSelected,
    PauseAll,
    ResumeSelected,
    ResumeAll,
    SelectNext,
    SelectPrev,
    NewProject,
    OpenProject,
    SaveProject,
    AddFile,
    DeleteSelected,
    RenameSelected,
    MoveUp,
    MoveDown,
}

impl AppAction {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Go             => "GO (play selected)",
            Self::StopSelected   => "Stop selected",
            Self::StopAll        => "Stop all",
            Self::PauseSelected  => "Pause selected",
            Self::PauseAll       => "Pause all",
            Self::ResumeSelected => "Resume selected",
            Self::ResumeAll      => "Resume all",
            Self::SelectNext     => "Select next",
            Self::SelectPrev     => "Select previous",
            Self::NewProject     => "New project",
            Self::OpenProject    => "Open project",
            Self::SaveProject    => "Save project",
            Self::AddFile        => "Add file(s)",
            Self::DeleteSelected => "Delete selected",
            Self::RenameSelected => "Rename selected",
            Self::MoveUp         => "Move cue up",
            Self::MoveDown       => "Move cue down",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeyConfig {
    pub bindings: HashMap<AppAction, String>,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        let mut bindings = HashMap::new();
        bindings.insert(AppAction::Go,             "Space".to_string());
        bindings.insert(AppAction::StopSelected,   "S".to_string());
        bindings.insert(AppAction::StopAll,        "Escape".to_string());
        bindings.insert(AppAction::PauseSelected,  "P".to_string());
        bindings.insert(AppAction::PauseAll,       "Shift+P".to_string());
        bindings.insert(AppAction::ResumeSelected, "R".to_string());
        bindings.insert(AppAction::ResumeAll,      "Shift+R".to_string());
        bindings.insert(AppAction::SelectNext,     "Down".to_string());
        bindings.insert(AppAction::SelectPrev,     "Up".to_string());
        bindings.insert(AppAction::SaveProject,    "Ctrl+S".to_string());
        bindings.insert(AppAction::OpenProject,    "Ctrl+O".to_string());
        bindings.insert(AppAction::NewProject,     "Ctrl+N".to_string());
        bindings.insert(AppAction::AddFile,        "Ctrl+I".to_string());
        bindings.insert(AppAction::DeleteSelected, "Delete".to_string());
        bindings.insert(AppAction::MoveUp,         "Alt+Up".to_string());
        bindings.insert(AppAction::MoveDown,       "Alt+Down".to_string());
        Self { bindings }
    }
}
