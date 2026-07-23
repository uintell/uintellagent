//! Private, project-scoped prompt history for CLI and TUI input recall.

use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const HISTORY_VERSION: u32 = 1;
const MAX_ENTRIES: usize = 500;
const MAX_ENTRY_BYTES: usize = 16 * 1024;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct HistoryEntry {
    timestamp_unix: u64,
    workspace: String,
    text: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct HistoryDocument {
    version: u32,
    entries: Vec<HistoryEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordOutcome {
    Stored,
    SkippedSensitive,
    SkippedTooLarge,
    SkippedEmpty,
}

pub struct PromptHistory {
    path: PathBuf,
    workspace: String,
    entries: Vec<HistoryEntry>,
    navigation_index: Option<usize>,
    navigation_prefix: String,
    navigation_draft: String,
}

impl PromptHistory {
    pub fn load_current() -> io::Result<Self> {
        let path = history_path()?;
        let workspace = workspace_key(&std::env::current_dir()?);
        Self::load_at(path, workspace)
    }

    pub fn empty_current() -> Self {
        let path = history_path().unwrap_or_else(|_| PathBuf::from(".uintell-prompt-history.json"));
        let workspace = std::env::current_dir()
            .map(|path| workspace_key(&path))
            .unwrap_or_else(|_| ".".into());
        Self::empty_at(path, workspace)
    }

    fn empty_at(path: PathBuf, workspace: String) -> Self {
        Self {
            path,
            workspace,
            entries: Vec::new(),
            navigation_index: None,
            navigation_prefix: String::new(),
            navigation_draft: String::new(),
        }
    }

    fn load_at(path: PathBuf, workspace: String) -> io::Result<Self> {
        if !path.exists() {
            return Ok(Self::empty_at(path, workspace));
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "prompt history must not be a symbolic link",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "prompt history exceeds 8 MiB",
            ));
        }
        let document: HistoryDocument = serde_json::from_slice(&fs::read(&path)?)?;
        if document.version != HISTORY_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported prompt history version {}", document.version),
            ));
        }
        let entries = document
            .entries
            .into_iter()
            .filter(|entry| {
                !entry.text.trim().is_empty()
                    && entry.text.len() <= MAX_ENTRY_BYTES
                    && !entry.workspace.is_empty()
            })
            .rev()
            .take(MAX_ENTRIES)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        Ok(Self {
            path,
            workspace,
            entries,
            navigation_index: None,
            navigation_prefix: String::new(),
            navigation_draft: String::new(),
        })
    }

    pub fn record(&mut self, text: &str) -> io::Result<RecordOutcome> {
        self.reset_navigation();
        let text = text.trim();
        if text.is_empty() {
            return Ok(RecordOutcome::SkippedEmpty);
        }
        if text.len() > MAX_ENTRY_BYTES {
            return Ok(RecordOutcome::SkippedTooLarge);
        }
        if looks_sensitive(text) {
            return Ok(RecordOutcome::SkippedSensitive);
        }
        self.entries
            .retain(|entry| entry.workspace != self.workspace || entry.text != text);
        self.entries.push(HistoryEntry {
            timestamp_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            workspace: self.workspace.clone(),
            text: text.to_string(),
        });
        if self.entries.len() > MAX_ENTRIES {
            self.entries.drain(..self.entries.len() - MAX_ENTRIES);
        }
        self.persist()?;
        Ok(RecordOutcome::Stored)
    }

    pub fn suggestion(&self, prefix: &str) -> Option<&str> {
        if prefix.is_empty() || prefix.contains('\n') {
            return None;
        }
        self.entries
            .iter()
            .rev()
            .find(|entry| {
                entry.workspace == self.workspace
                    && entry.text.len() > prefix.len()
                    && entry.text.starts_with(prefix)
            })
            .map(|entry| entry.text.as_str())
    }

    pub fn previous(&mut self, current: &str) -> Option<String> {
        let start = if let Some(index) = self.navigation_index {
            index
        } else {
            self.navigation_prefix = current.to_string();
            self.navigation_draft = current.to_string();
            self.entries.len()
        };
        for index in (0..start).rev() {
            let entry = &self.entries[index];
            if entry.workspace == self.workspace
                && (self.navigation_prefix.is_empty()
                    || entry.text.starts_with(&self.navigation_prefix))
            {
                self.navigation_index = Some(index);
                return Some(entry.text.clone());
            }
        }
        None
    }

    pub fn next(&mut self) -> Option<String> {
        let current = self.navigation_index?;
        for index in current + 1..self.entries.len() {
            let entry = &self.entries[index];
            if entry.workspace == self.workspace
                && (self.navigation_prefix.is_empty()
                    || entry.text.starts_with(&self.navigation_prefix))
            {
                self.navigation_index = Some(index);
                return Some(entry.text.clone());
            }
        }
        self.navigation_index = None;
        Some(self.navigation_draft.clone())
    }

    pub fn reset_navigation(&mut self) {
        self.navigation_index = None;
        self.navigation_prefix.clear();
        self.navigation_draft.clear();
    }

    pub fn recent(&self, limit: usize) -> Vec<&str> {
        self.entries
            .iter()
            .rev()
            .filter(|entry| entry.workspace == self.workspace)
            .take(limit)
            .map(|entry| entry.text.as_str())
            .collect()
    }

    pub fn clear_current(&mut self) -> io::Result<usize> {
        let before = self.entries.len();
        self.entries
            .retain(|entry| entry.workspace != self.workspace);
        let removed = before - self.entries.len();
        self.reset_navigation();
        self.persist()?;
        Ok(removed)
    }

    fn persist(&self) -> io::Result<()> {
        let parent = self.path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "history path has no parent")
        })?;
        fs::create_dir_all(parent)?;
        let document = HistoryDocument {
            version: HISTORY_VERSION,
            entries: self.entries.clone(),
        };
        let contents = serde_json::to_vec(&document)?;
        let temporary = parent.join(format!(
            ".prompt-history-{}-{:016x}.tmp",
            std::process::id(),
            rand::random::<u64>()
        ));
        let result = (|| -> io::Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(&contents)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}

pub fn record_current(text: &str) -> io::Result<RecordOutcome> {
    let mut history = PromptHistory::load_current()?;
    history.record(text)
}

fn history_path() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("UINTELL_PROMPT_HISTORY") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(home.join(".uintell").join("prompt-history.json"))
}

fn workspace_key(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn looks_sensitive(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("-----begin ") && lower.contains(" private key-----") {
        return true;
    }
    const MARKERS: &[&str] = &[
        "-----begin private key",
        "-----begin pgp private key",
        "-----begin openssh private key",
        "authorization: bearer ",
        "api_key=",
        "api_key:",
        "api-key=",
        "api-key:",
        "apikey=",
        "api key:",
        "api key=",
        "password=",
        "password:",
        "passwd=",
        "secret=",
        "secret:",
        "token=",
        "access_token",
        "refresh_token",
        "seed phrase",
        "seed:",
        "mnemonic:",
        "recovery phrase",
        "private key:",
        "private_key=",
        "nsec1",
    ];
    MARKERS.iter().any(|marker| lower.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> (tempfile::TempDir, PromptHistory) {
        let directory = tempfile::tempdir().unwrap();
        let history = PromptHistory::load_at(
            directory.path().join("prompt-history.json"),
            "/workspace/a".into(),
        )
        .unwrap();
        (directory, history)
    }

    #[test]
    fn suggestions_and_navigation_are_project_scoped() {
        let (_directory, mut history) = history();
        history.record("cargo test --all-targets").unwrap();
        history.record("cargo clippy --all-targets").unwrap();
        history.entries.push(HistoryEntry {
            timestamp_unix: 1,
            workspace: "/workspace/b".into(),
            text: "cargo publish".into(),
        });

        assert_eq!(
            history.suggestion("cargo t"),
            Some("cargo test --all-targets")
        );
        assert_eq!(
            history.previous("cargo").as_deref(),
            Some("cargo clippy --all-targets")
        );
        assert_eq!(
            history.previous("cargo").as_deref(),
            Some("cargo test --all-targets")
        );
        assert_eq!(
            history.next().as_deref(),
            Some("cargo clippy --all-targets")
        );
        assert_eq!(history.next().as_deref(), Some("cargo"));
    }

    #[test]
    fn secrets_and_oversized_prompts_are_not_persisted() {
        let (directory, mut history) = history();
        assert_eq!(
            history.record("password=hunter2").unwrap(),
            RecordOutcome::SkippedSensitive
        );
        assert_eq!(
            history.record("use nsec1qqqqqqqqqqqq").unwrap(),
            RecordOutcome::SkippedSensitive
        );
        assert_eq!(
            history.record(&"x".repeat(MAX_ENTRY_BYTES + 1)).unwrap(),
            RecordOutcome::SkippedTooLarge
        );
        assert!(!directory.path().join("prompt-history.json").exists());
    }

    #[test]
    fn history_is_private_atomic_and_reloadable() {
        let (directory, mut history) = history();
        history.record("remember this prompt").unwrap();
        let path = directory.path().join("prompt-history.json");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let loaded = PromptHistory::load_at(path, "/workspace/a".into()).unwrap();
        assert_eq!(loaded.recent(10), vec!["remember this prompt"]);
    }

    #[test]
    fn clearing_only_removes_the_current_project() {
        let (_directory, mut history) = history();
        history.record("local prompt").unwrap();
        history.entries.push(HistoryEntry {
            timestamp_unix: 1,
            workspace: "/workspace/b".into(),
            text: "other prompt".into(),
        });
        assert_eq!(history.clear_current().unwrap(), 1);
        assert!(history.recent(10).is_empty());
        assert_eq!(history.entries.len(), 1);
    }
}
