//! Persistent prompt history with readline-style recall — see
//! meta/issues/prompt-history-recall.md.
//!
//! Entries are stored one JSON string per line (newline-safe) in the
//! state dir and shared across sessions. The file is rewritten bounded on
//! every push; concurrent ilar instances race benignly (last writer wins).

use std::path::PathBuf;

const MAX_HISTORY_ENTRIES: usize = 1000;

pub struct PromptHistory {
    path: Option<PathBuf>,
    entries: Vec<String>,
    /// Index into `entries` while browsing; `None` when not recalling.
    cursor: Option<usize>,
    /// The in-progress input stashed when browsing started (or the last
    /// edit made to a recalled entry), restored by moving past the end.
    draft: String,
}

impl PromptHistory {
    pub fn in_memory() -> Self {
        Self {
            path: None,
            entries: Vec::new(),
            cursor: None,
            draft: String::new(),
        }
    }

    pub fn load(path: PathBuf) -> Self {
        let mut entries: Vec<String> = std::fs::read_to_string(&path)
            .map(|content| {
                content
                    .lines()
                    .filter_map(|line| serde_json::from_str(line).ok())
                    .collect()
            })
            .unwrap_or_default();
        if entries.len() > MAX_HISTORY_ENTRIES {
            entries.drain(..entries.len() - MAX_HISTORY_ENTRIES);
        }
        Self {
            path: Some(path),
            entries,
            cursor: None,
            draft: String::new(),
        }
    }

    pub fn browsing(&self) -> bool {
        self.cursor.is_some()
    }

    /// Move to the previous (older) entry. `current` is the input's text,
    /// stashed as the draft when browsing starts or when it was edited.
    pub fn previous(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let next_index = match self.cursor {
            None => {
                self.draft = current.to_string();
                self.entries.len() - 1
            }
            Some(0) => return None,
            Some(index) => {
                if self.entries[index] != current {
                    self.draft = current.to_string();
                }
                index - 1
            }
        };
        self.cursor = Some(next_index);
        Some(self.entries[next_index].clone())
    }

    /// Move to the next (newer) entry; past the newest, restore the draft
    /// and stop browsing.
    pub fn next(&mut self, current: &str) -> Option<String> {
        let index = self.cursor?;
        if self.entries[index] != current {
            self.draft = current.to_string();
        }
        if index + 1 < self.entries.len() {
            self.cursor = Some(index + 1);
            Some(self.entries[index + 1].clone())
        } else {
            self.cursor = None;
            Some(std::mem::take(&mut self.draft))
        }
    }

    /// Record a submitted prompt and reset any browsing state.
    pub fn push(&mut self, prompt: &str) {
        self.cursor = None;
        self.draft.clear();
        if prompt.trim().is_empty() || self.entries.last().is_some_and(|last| last == prompt) {
            return;
        }
        self.entries.push(prompt.to_string());
        if self.entries.len() > MAX_HISTORY_ENTRIES {
            let excess = self.entries.len() - MAX_HISTORY_ENTRIES;
            self.entries.drain(..excess);
        }
        self.persist();
    }

    fn persist(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut output = String::new();
        for entry in &self.entries {
            if let Ok(line) = serde_json::to_string(entry) {
                output.push_str(&line);
                output.push('\n');
            }
        }
        let _ = std::fs::write(path, output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history_with(entries: &[&str]) -> PromptHistory {
        let mut history = PromptHistory::in_memory();
        for entry in entries {
            history.push(entry);
        }
        history
    }

    #[test]
    fn recalls_newest_first_and_restores_draft() {
        let mut history = history_with(&["one", "two"]);
        assert!(!history.browsing());
        assert_eq!(history.previous("draft"), Some("two".into()));
        assert!(history.browsing());
        assert_eq!(history.previous("two"), Some("one".into()));
        assert_eq!(history.previous("one"), None, "oldest entry stays put");
        assert_eq!(history.next("one"), Some("two".into()));
        assert_eq!(history.next("two"), Some("draft".into()));
        assert!(!history.browsing());
    }

    #[test]
    fn edits_to_a_recalled_entry_become_the_draft() {
        let mut history = history_with(&["one", "two"]);
        assert_eq!(history.previous(""), Some("two".into()));
        // The user edits "two" into "two edited", then keeps browsing.
        assert_eq!(history.previous("two edited"), Some("one".into()));
        assert_eq!(history.next("one"), Some("two".into()));
        assert_eq!(history.next("two"), Some("two edited".into()));
    }

    #[test]
    fn push_dedups_consecutive_skips_blank_and_resets_browsing() {
        let mut history = history_with(&["same", "same", "  ", "other"]);
        assert_eq!(history.entries, vec!["same", "other"]);
        history.previous("");
        history.push("newest");
        assert!(!history.browsing());
        assert_eq!(history.previous(""), Some("newest".into()));
    }

    #[test]
    fn history_is_bounded() {
        let mut history = PromptHistory::in_memory();
        for index in 0..(MAX_HISTORY_ENTRIES + 10) {
            history.push(&format!("prompt {index}"));
        }
        assert_eq!(history.entries.len(), MAX_HISTORY_ENTRIES);
        assert_eq!(history.entries.first().unwrap(), "prompt 10");
    }

    #[test]
    fn persists_and_reloads_multiline_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("prompt_history.jsonl");
        let mut history = PromptHistory::load(path.clone());
        history.push("line one\nline two");
        history.push("plain");

        let mut reloaded = PromptHistory::load(path.clone());
        assert_eq!(reloaded.previous(""), Some("plain".into()));
        assert_eq!(
            reloaded.previous("plain"),
            Some("line one\nline two".into())
        );

        // Corrupt lines are skipped, valid ones survive.
        std::fs::write(
            &path,
            format!("garbage\n{}\n", serde_json::to_string("kept").unwrap()),
        )
        .unwrap();
        let mut tolerant = PromptHistory::load(path);
        assert_eq!(tolerant.previous(""), Some("kept".into()));
    }

    #[test]
    fn empty_history_recall_is_inert() {
        let mut history = PromptHistory::in_memory();
        assert_eq!(history.previous("draft"), None);
        assert!(!history.browsing());
        assert_eq!(history.next("draft"), None);
    }
}
