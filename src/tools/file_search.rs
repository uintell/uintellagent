// File Search tool — ripgrep-style content search across files
use rig_core::completion::ToolDefinition;
use rig_core::tool::Tool;
use serde::Deserialize;
use serde_json::json;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Deserialize)]
pub struct FileSearchArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    file_glob: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct FileSearchError {
    message: String,
}

impl FileSearchError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub struct FileSearch;

impl Tool for FileSearch {
    const NAME: &'static str = "file_search";

    type Error = FileSearchError;
    type Args = FileSearchArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "file_search".to_string(),
            description: "Search file contents using ripgrep. Use for finding code, patterns, or text across the project.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex pattern to search for" },
                    "path": { "type": "string", "description": "Directory to search (default: .)" },
                    "file_glob": { "type": "string", "description": "File filter, e.g. '*.rs' for Rust files" },
                    "limit": { "type": "integer", "description": "Max results (default 30)" }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = args.path.unwrap_or_else(|| ".".into());
        let permission_args = json!({ "path": &path }).to_string();
        if let Err(reason) = crate::permissions::enforce_tool_call(Self::NAME, &permission_args) {
            return Ok(reason);
        }

        let limit = args.limit.unwrap_or(30).clamp(1, 500);
        let limit_str = limit.to_string();

        let mut cmd = Command::new("rg");
        cmd.arg("--line-number")
            .arg("--max-count")
            .arg(&limit_str)
            .arg("--no-heading")
            .arg("--color")
            .arg("never");

        if let Some(glob) = &args.file_glob {
            cmd.arg("--glob").arg(glob);
        }

        cmd.arg("--");
        cmd.arg(&args.pattern);
        cmd.arg(&path);

        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = cmd
            .spawn()
            .map_err(|error| FileSearchError::new(format!("start ripgrep: {error}")))?;
        let output = match timeout(Duration::from_secs(20), child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                return Err(FileSearchError::new(format!("wait for ripgrep: {error}")))
            }
            Err(_) => return Ok("file_search timed out after 20s".into()),
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        if output.status.code() == Some(1) {
            return Ok(format!("No matches for '{}' in {path}", args.pattern));
        }
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(FileSearchError::new(format!(
                "ripgrep failed with {}: {}",
                output.status,
                stderr.trim()
            )));
        }
        if stdout.trim().is_empty() {
            return Ok(format!("No matches for '{}' in {path}", args.pattern));
        }

        // Truncate
        let lines: Vec<&str> = stdout.lines().take(limit).collect();
        let mut out = format!("Results for '{}' in {path}:\n\n", args.pattern);
        for line in &lines {
            out.push_str(line);
            out.push('\n');
        }
        if stdout.lines().count() > limit {
            out.push_str(&format!(
                "... ({} more results)",
                stdout.lines().count() - limit
            ));
        }
        Ok(out)
    }
}
