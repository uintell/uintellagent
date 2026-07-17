// File tools — read and write files
use rig_core::completion::ToolDefinition;
use rig_core::tool::Tool;
use serde::Deserialize;
use serde_json::json;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    pub path: PathBuf,
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
}

static FILE_CHANGES: LazyLock<Mutex<Vec<FileChange>>> = LazyLock::new(|| Mutex::new(Vec::new()));
const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;

pub(crate) fn take_file_changes() -> Vec<FileChange> {
    FILE_CHANGES
        .lock()
        .map(|mut changes| std::mem::take(&mut *changes))
        .unwrap_or_default()
}

fn publish_file_change(path: &Path, before: Option<Vec<u8>>, after: Vec<u8>) {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    if let Ok(mut changes) = FILE_CHANGES.lock() {
        if let Some(existing) = changes
            .iter_mut()
            .rev()
            .find(|change| change.path == absolute && before.as_deref() == change.after.as_deref())
        {
            existing.after = Some(after);
        } else {
            changes.push(FileChange {
                path: absolute,
                before,
                after: Some(after),
            });
        }
    }
}

pub(crate) fn write_review_result(
    path: &Path,
    expected: Option<&[u8]>,
    result: Option<&[u8]>,
) -> Result<(), String> {
    let current = std::fs::read(path).ok();
    if current.as_deref() != expected {
        return Err(format!(
            "{} changed again after the review opened; reload before resolving it",
            path.display()
        ));
    }
    match result {
        Some(content) => atomic_write(path, content).map_err(|error| error.to_string()),
        None if path.exists() => std::fs::remove_file(path)
            .map_err(|error| format!("remove {}: {error}", path.display())),
        None => Ok(()),
    }
}

// ── File Read ──────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct FileReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct FileReadError {
    message: String,
}

pub struct FileRead;

impl Tool for FileRead {
    const NAME: &'static str = "file_read";

    type Error = FileReadError;
    type Args = FileReadArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "file_read".to_string(),
            description: "Read a file with line numbers. Use offset and limit for large files."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file" },
                    "offset": { "type": "integer", "description": "Start line (1-indexed)" },
                    "limit": { "type": "integer", "description": "Max lines to read" }
                },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let permission_args = json!({ "path": &args.path }).to_string();
        if let Err(reason) = crate::permissions::enforce_tool_call(Self::NAME, &permission_args) {
            return Ok(reason);
        }

        let file = std::fs::File::open(&args.path).map_err(|error| FileReadError {
            message: format!("open {}: {error}", args.path),
        })?;
        let mut bytes = Vec::new();
        file.take((MAX_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| FileReadError {
                message: format!("read {}: {error}", args.path),
            })?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(FileReadError {
                message: format!(
                    "{} exceeds the {} byte file_read limit",
                    args.path, MAX_FILE_BYTES
                ),
            });
        }
        let content = String::from_utf8(bytes).map_err(|_| FileReadError {
            message: format!("{} is not valid UTF-8 text", args.path),
        })?;
        let lines: Vec<&str> = content.lines().collect();

        let start = args.offset.unwrap_or(1).saturating_sub(1).min(lines.len());
        let end = args
            .limit
            .map(|l| (start + l).min(lines.len()))
            .unwrap_or(lines.len());

        let mut output = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            output.push_str(&format!("{}|{}\n", start + i + 1, line));
        }
        Ok(output)
    }
}

// ── File Write ─────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct FileWriteArgs {
    path: String,
    content: String,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct FileWriteError {
    message: String,
}

impl FileWriteError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub struct FileWrite;

impl Tool for FileWrite {
    const NAME: &'static str = "file_write";

    type Error = FileWriteError;
    type Args = FileWriteArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "file_write".to_string(),
            description: "Create or overwrite a file. Creates parent directories automatically."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to write" },
                    "content": { "type": "string", "description": "Content to write" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = Path::new(&args.path);
        let permission_args = json!({ "path": &args.path }).to_string();
        if let Err(reason) = crate::permissions::enforce_tool_call(Self::NAME, &permission_args) {
            return Ok(reason);
        }
        if args.content.len() > MAX_FILE_BYTES {
            return Err(FileWriteError::new(format!(
                "content exceeds the {MAX_FILE_BYTES} byte file_write limit"
            )));
        }

        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                FileWriteError::new(format!("create {}: {error}", parent.display()))
            })?;
        }
        let before = std::fs::read(path).ok();
        let after = args.content.into_bytes();
        atomic_write(path, &after)?;
        publish_file_change(path, before, after.clone());
        Ok(format!("Wrote {} bytes to {}", after.len(), args.path))
    }
}

fn atomic_write(path: &Path, content: &[u8]) -> Result<(), FileWriteError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let directory = parent.unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temporary = directory.join(format!(
        ".{name}.uintell-{}-{}.tmp",
        std::process::id(),
        rand::random::<u64>()
    ));

    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(content)?;
        file.sync_all()?;
        if let Ok(metadata) = std::fs::metadata(path) {
            std::fs::set_permissions(&temporary, metadata.permissions())?;
        }
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(FileWriteError::new(format!(
            "atomic write {}: {error}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_complete_content_and_preserves_mode() {
        let path = std::env::temp_dir().join(format!(
            "uintell-file-write-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::write(&path, "old").unwrap();
        let permissions = std::fs::metadata(&path).unwrap().permissions();

        atomic_write(&path, b"new complete content").unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "new complete content"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().readonly(),
            permissions.readonly()
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn review_write_detects_conflicts_and_can_restore_content() {
        let path = std::env::temp_dir().join(format!(
            "uintell-file-review-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::write(&path, "agent version").unwrap();

        write_review_result(&path, Some(b"agent version"), Some(b"reviewed version")).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "reviewed version");

        let error = write_review_result(&path, Some(b"agent version"), Some(b"old")).unwrap_err();
        assert!(error.contains("changed again"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn consecutive_file_changes_are_coalesced_without_losing_original() {
        let path = std::env::temp_dir().join(format!(
            "uintell-file-change-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let _ = take_file_changes();
        publish_file_change(&path, Some(b"first".to_vec()), b"second".to_vec());
        publish_file_change(&path, Some(b"second".to_vec()), b"third".to_vec());

        let changes = take_file_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].before.as_deref(), Some(b"first".as_slice()));
        assert_eq!(changes[0].after.as_deref(), Some(b"third".as_slice()));
    }
}
