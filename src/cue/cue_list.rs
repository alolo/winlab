// src/cue/cue_list.rs — список кью с поддержкой иерархической структуры групп

use crate::cue::{Cue, CueState, CueType};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CueList {
    pub name: String,
    pub cues: Vec<Cue>,
}

impl CueList {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            cues: vec![],
        }
    }

    pub fn add(&mut self, cue: Cue) {
        self.cues.push(cue);
        self.renumber();
    }

    pub fn remove(&mut self, id: Uuid) {
        self.cues.retain(|c| c.id != id);
        for c in &mut self.cues {
            if let CueType::Group { children, .. } = &mut c.cue_type {
                children.retain(|cid| *cid != id);
            }
        }
        self.renumber();
    }

    pub fn find(&self, id: Uuid) -> Option<&Cue> {
        self.cues.iter().find(|c| c.id == id)
    }

    pub fn find_mut(&mut self, id: Uuid) -> Option<&mut Cue> {
        self.cues.iter_mut().find(|c| c.id == id)
    }

    /// Возвращает список Uuid строго в порядке визуальной структуры (сверху вниз с учетом групп)
    pub fn visual_order_ids(&self) -> Vec<Uuid> {
        let child_ids: HashSet<Uuid> = self
            .cues
            .iter()
            .filter_map(|c| match &c.cue_type {
                CueType::Group { children, .. } => Some(children.iter().copied()),
                _ => None,
            })
            .flatten()
            .collect();

        let mut order = Vec::with_capacity(self.cues.len());
        for cue in &self.cues {
            if child_ids.contains(&cue.id) {
                continue;
            }
            order.push(cue.id);
            if let CueType::Group { children, .. } = &cue.cue_type {
                for &cid in children {
                    if self.cues.iter().any(|c| c.id == cid) {
                        order.push(cid);
                    }
                }
            }
        }
        order
    }

    /// Следующее событие строго по структуре (ниже по списку)
    pub fn next_in_structure(&self, current_id: Uuid) -> Option<Uuid> {
        // 1. Если current_id находится внутри какой-то группы, проверяем следующий элемент в этой группе
        for cue in &self.cues {
            if let CueType::Group { children, .. } = &cue.cue_type {
                if let Some(pos) = children.iter().position(|&cid| cid == current_id) {
                    if let Some(&next_child) = children.get(pos + 1) {
                        return Some(next_child);
                    } else {
                        // Это был последний элемент группы — берем следующее событие верхнего уровня после самой группы
                        return self.next_top_level_after(cue.id);
                    }
                }
            }
        }

        // 2. Иначе ищем в общем структурном порядке
        let order = self.visual_order_ids();
        let pos = order.iter().position(|&id| id == current_id)?;
        order.get(pos + 1).copied()
    }

    fn next_top_level_after(&self, group_id: Uuid) -> Option<Uuid> {
        let child_ids: HashSet<Uuid> = self
            .cues
            .iter()
            .filter_map(|c| match &c.cue_type {
                CueType::Group { children, .. } => Some(children.iter().copied()),
                _ => None,
            })
            .flatten()
            .collect();

        let top_level: Vec<Uuid> = self
            .cues
            .iter()
            .filter(|c| !child_ids.contains(&c.id))
            .map(|c| c.id)
            .collect();

        let pos = top_level.iter().position(|&id| id == group_id)?;
        top_level.get(pos + 1).copied()
    }

    /// Перенумеровывает события строго сверху вниз по структуре
    pub fn renumber(&mut self) {
        let order = self.visual_order_ids();
        for (num_idx, id) in order.into_iter().enumerate() {
            if let Some(cue) = self.cues.iter_mut().find(|c| c.id == id) {
                cue.number = format!("{}", num_idx + 1);
            }
        }
    }

    pub fn active_count(&self) -> usize {
        self.cues.iter().filter(|c| c.is_active()).count()
    }

    pub fn pause_all(&mut self) {
        for c in &mut self.cues {
            if matches!(
                c.state,
                CueState::Playing | CueState::FadingIn | CueState::FadingOut
            ) {
                c.state = CueState::Paused;
            }
        }
    }

    pub fn resume_all(&mut self) {
        for c in &mut self.cues {
            if c.state == CueState::Paused {
                c.state = CueState::Playing;
            }
        }
    }

    pub fn stop_all(&mut self) {
        for c in &mut self.cues {
            c.state = CueState::Stopped;
            c.position_secs = 0.0;
        }
    }

    pub fn move_up(&mut self, id: Uuid) {
        // Если элемент внутри группы — двигаем его внутри списка children этой группы
        for c in &mut self.cues {
            if let CueType::Group { children, .. } = &mut c.cue_type {
                if let Some(pos) = children.iter().position(|&cid| cid == id) {
                    if pos > 0 {
                        children.swap(pos, pos - 1);
                    }
                    self.renumber();
                    return;
                }
            }
        }
        if let Some(i) = self.cues.iter().position(|c| c.id == id) {
            if i > 0 {
                self.cues.swap(i, i - 1);
                self.renumber();
            }
        }
    }

    pub fn move_down(&mut self, id: Uuid) {
        for c in &mut self.cues {
            if let CueType::Group { children, .. } = &mut c.cue_type {
                if let Some(pos) = children.iter().position(|&cid| cid == id) {
                    if pos + 1 < children.len() {
                        children.swap(pos, pos + 1);
                    }
                    self.renumber();
                    return;
                }
            }
        }
        let len = self.cues.len();
        if let Some(i) = self.cues.iter().position(|c| c.id == id) {
            if i + 1 < len {
                self.cues.swap(i, i + 1);
                self.renumber();
            }
        }
    }

    pub fn audio_targets(&self) -> Vec<(Uuid, String, std::path::PathBuf)> {
        self.cues
            .iter()
            .filter(|c| {
                matches!(&c.cue_type, CueType::Audio) || !c.file_path.as_os_str().is_empty()
            })
            .map(|c| {
                let fname = c
                    .file_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("no file");
                (
                    c.id,
                    format!("#{} — {} ({})", c.number, c.name, fname),
                    c.file_path.clone(),
                )
            })
            .collect()
    }
}
