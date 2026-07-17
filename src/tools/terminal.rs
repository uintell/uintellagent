// Terminal tool — persistent shell sessions with timeouts and structured output
//
// Uses session.rs for long-lived bash process. State (cd, env vars,
// venv activations) persists across calls.
//
// Structured output: every result includes exit_code, stdout, stderr, elapsed,
// and a success/error flag the agent can branch on.

use rig_core::completion::ToolDefinition;
use rig_core::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Deserialize)]
pub struct TerminalArgs {
    command: String,
    /// Timeout in seconds (default 30, max 300)
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
    /// If true, restart the session (fresh shell, loses state)
    #[serde(default)]
    restart: bool,
}

fn default_timeout() -> u64 {
    30
}

#[derive(Serialize)]
struct TerminalOutput {
    success: bool,
    exit_code: i32,
    stdout: String,
    stderr: String,
    elapsed_ms: u64,
    cwd: String,
    truncated: bool,
    timeout: bool,
    error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("terminal error")]
pub struct TerminalError;

pub struct Terminal;

impl Tool for Terminal {
    const NAME: &'static str = "terminal";

    type Error = TerminalError;
    type Args = TerminalArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "terminal".to_string(),
            description: "Execute a shell command in a persistent session. State (cd, exports, venv) survives between calls. Use 'restart: true' for a fresh shell. Default timeout 30s, max 300s.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 30, max 300)" },
                    "restart": { "type": "boolean", "description": "Restart the session (fresh shell)" }
                },
                "required": ["command"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let permission_args = json!({ "command": &args.command }).to_string();
        let command =
            match crate::permissions::authorize_terminal_command(&args.command, &permission_args) {
                Ok(command) => command,
                Err(reason) => {
                    let output = TerminalOutput {
                        success: false,
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: String::new(),
                        elapsed_ms: 0,
                        cwd: String::new(),
                        truncated: false,
                        timeout: false,
                        error: Some(reason),
                    };
                    return Ok(serde_json::to_string_pretty(&output).unwrap_or_default());
                }
            };

        let timeout_secs = args.timeout_secs.clamp(1, 300);

        // Ensure session exists
        if args.restart {
            let _ = crate::session::restart_session().await;
        } else {
            let _ = crate::session::get_or_create_session().await;
        }

        // Execute
        match crate::session::exec(&command, timeout_secs).await {
            Ok(result) => {
                let stdout_truncated = result.stdout.contains("(truncated at 50000 bytes;");
                let stderr_truncated = result.stderr.contains("(truncated at 10000 bytes;");
                let output = TerminalOutput {
                    success: result.exit_code == 0,
                    exit_code: result.exit_code,
                    stdout: result.stdout,
                    stderr: result.stderr,
                    elapsed_ms: result.elapsed.as_millis() as u64,
                    cwd: result.cwd,
                    truncated: stdout_truncated || stderr_truncated,
                    timeout: false,
                    error: None,
                };
                Ok(serde_json::to_string_pretty(&output).unwrap_or_default())
            }
            Err(crate::session::ExecError::Timeout(secs)) => {
                let output = TerminalOutput {
                    success: false,
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: String::new(),
                    elapsed_ms: secs * 1000,
                    cwd: crate::session::get_cwd().await.unwrap_or_default(),
                    truncated: false,
                    timeout: true,
                    error: Some(format!("Command timed out after {secs}s")),
                };
                Ok(serde_json::to_string_pretty(&output).unwrap_or_default())
            }
            Err(e) => {
                let output = TerminalOutput {
                    success: false,
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: String::new(),
                    elapsed_ms: 0,
                    cwd: String::new(),
                    truncated: false,
                    timeout: false,
                    error: Some(format!("Session error: {e}")),
                };
                Ok(serde_json::to_string_pretty(&output).unwrap_or_default())
            }
        }
    }
}
