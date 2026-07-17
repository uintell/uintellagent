// Code Execution tool — run short scripts in an isolated temp working directory
use rig_core::completion::ToolDefinition;
use rig_core::tool::Tool;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Deserialize)]
pub struct CodeExecArgs {
    code: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
}

fn default_timeout() -> u64 {
    30
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct CodeExecError {
    message: String,
}

impl CodeExecError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub struct CodeExec;

pub(crate) async fn self_test() -> Result<(), String> {
    let python = exec_interpreter("python3", &["-c", "print('python-ok')"], 10)
        .await
        .map_err(|error| error.to_string())?;
    if !python.contains("python-ok") || !compile_success(&python) {
        return Err(format!("Python sandbox failed: {python}"));
    }
    let rust = exec_rust("fn main() { println!(\"rust-ok\"); }", 20)
        .await
        .map_err(|error| error.to_string())?;
    if !rust.contains("rust-ok") || !compile_success(&rust) {
        return Err(format!("Rust sandbox failed: {rust}"));
    }
    Ok(())
}

impl Tool for CodeExec {
    const NAME: &'static str = "code_exec";

    type Error = CodeExecError;
    type Args = CodeExecArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "code_exec".to_string(),
            description: "Execute code. Supports: python (default), bash, rust (via rustc), node. Returns stdout+stderr. Timeout: 30s. Use for calculations, data processing, quick scripts.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "Code to execute" },
                    "language": { "type": "string", "description": "python, bash, rust, node (default: python)" },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 30, max 120)" }
                },
                "required": ["code"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let permission_args = json!({
            "code": &args.code,
            "language": args.language.as_deref().unwrap_or("python"),
        })
        .to_string();
        if let Err(reason) = crate::permissions::enforce_tool_call(Self::NAME, &permission_args) {
            return Ok(reason);
        }

        let lang = args.language.as_deref().unwrap_or("python");
        let timeout_secs = args.timeout_secs.clamp(1, 120);

        match lang {
            "python" | "py" => exec_interpreter("python3", &["-c", &args.code], timeout_secs).await,
            "bash" | "sh" => exec_interpreter("bash", &["-c", &args.code], timeout_secs).await,
            "rust" | "rs" => exec_rust(&args.code, timeout_secs).await,
            "node" | "js" => exec_interpreter("node", &["-e", &args.code], timeout_secs).await,
            _ => Ok(format!(
                "Unknown language: {lang}. Supported: python, bash, rust, node"
            )),
        }
    }
}

async fn exec_interpreter(
    program: &str,
    args: &[&str],
    timeout_secs: u64,
) -> Result<String, CodeExecError> {
    let dir = temp_run_dir("uintell_code");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|error| CodeExecError::new(format!("create execution directory: {error}")))?;
    let result = run_command(program, args, Some(&dir), timeout_secs).await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    result
}

async fn exec_rust(code: &str, timeout_secs: u64) -> Result<String, CodeExecError> {
    let dir = temp_run_dir("uintell_rust");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|error| CodeExecError::new(format!("create Rust directory: {error}")))?;

    let src = dir.join("main.rs");
    tokio::fs::write(&src, code)
        .await
        .map_err(|error| CodeExecError::new(format!("write main.rs: {error}")))?;

    let rustc = resolve_rustc()?;
    let compile = run_command(
        &rustc.to_string_lossy(),
        &["main.rs", "-o", "main"],
        Some(&dir),
        timeout_secs,
    )
    .await?;

    if !compile_success(&compile) {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        return Ok(format!("Compile error:\n{compile}"));
    }

    let run = run_command("./main", &[], Some(&dir), timeout_secs).await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    run
}

async fn run_command(
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
    timeout_secs: u64,
) -> Result<String, CodeExecError> {
    let mut command = sandboxed_command(program, args, cwd)?;

    let child = command
        .spawn()
        .map_err(|error| CodeExecError::new(format!("start {program}: {error}")))?;
    match timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(format_output(&output)),
        Ok(Err(error)) => Err(CodeExecError::new(format!("wait for {program}: {error}"))),
        Err(_) => Ok(format!("[timeout after {timeout_secs}s]\n[exit: -1]")),
    }
}

fn sandboxed_command(
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
) -> Result<Command, CodeExecError> {
    let mut command = if sandbox_enabled() {
        bubblewrap_command(program, args, cwd)?
    } else if unsandboxed_override_enabled() {
        let mut command = Command::new(program);
        command.args(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        command
    } else {
        return Err(CodeExecError::new(
            "code sandbox is disabled and UINTELL_ALLOW_UNSANDBOXED_CODE is not 1",
        ));
    };

    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(command)
}

fn bubblewrap_command(
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
) -> Result<Command, CodeExecError> {
    if !Path::new("/usr/bin/bwrap").exists() {
        return Err(CodeExecError::new(
            "bubblewrap is required for code execution; install bwrap or explicitly set UINTELL_ALLOW_UNSANDBOXED_CODE=1 with UINTELL_CODE_SANDBOX=0",
        ));
    }

    let workdir = cwd.unwrap_or_else(|| Path::new("/tmp"));
    let mut command = Command::new("/usr/bin/bwrap");
    command
        .arg("--die-with-parent")
        .arg("--unshare-all")
        .arg("--new-session")
        .arg("--proc")
        .arg("/proc")
        .arg("--dev")
        .arg("/dev")
        .arg("--tmpfs")
        .arg("/tmp")
        .arg("--dir")
        .arg("/run")
        .arg("--bind")
        .arg(workdir)
        .arg("/work")
        .arg("--chdir")
        .arg("/work")
        .arg("--setenv")
        .arg("HOME")
        .arg("/tmp")
        .arg("--setenv")
        .arg("PATH")
        .arg("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");

    bind_read_only_if_exists(&mut command, "/usr");
    bind_read_only_if_exists(&mut command, "/bin");
    bind_read_only_if_exists(&mut command, "/lib");
    bind_read_only_if_exists(&mut command, "/lib64");
    bind_read_only_if_exists(&mut command, "/etc/ssl/certs");
    bind_program_runtime(&mut command, program);

    command.arg("--").arg(program).args(args);
    Ok(command)
}

fn resolve_rustc() -> Result<PathBuf, CodeExecError> {
    if let Some(path) = std::env::var_os("RUSTC").map(PathBuf::from) {
        if path.is_absolute() && path.is_file() {
            return Ok(path);
        }
    }
    if let Ok(output) = std::process::Command::new("rustup")
        .args(["which", "rustc"])
        .output()
    {
        if output.status.success() {
            let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
            if path.is_absolute() && path.is_file() {
                return Ok(path);
            }
        }
    }
    find_in_path("rustc").ok_or_else(|| {
        CodeExecError::new("rustc is unavailable; install Rust or configure the RUSTC environment")
    })
}

fn find_in_path(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

fn bind_program_runtime(command: &mut Command, program: &str) {
    let path = Path::new(program);
    if !path.is_absolute()
        || path.starts_with("/usr")
        || path.starts_with("/bin")
        || path.starts_with("/sbin")
    {
        return;
    }
    if let Some(runtime_root) = path.parent().and_then(Path::parent) {
        if runtime_root.is_dir() {
            command.arg("--ro-bind").arg(runtime_root).arg(runtime_root);
        }
    }
}

fn bind_read_only_if_exists(command: &mut Command, path: &str) {
    if Path::new(path).exists() {
        command.arg("--ro-bind").arg(path).arg(path);
    }
}

fn sandbox_enabled() -> bool {
    std::env::var("UINTELL_CODE_SANDBOX").as_deref() != Ok("0")
}

fn unsandboxed_override_enabled() -> bool {
    std::env::var("UINTELL_ALLOW_UNSANDBOXED_CODE").as_deref() == Ok("1")
}

fn format_output(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit = output.status.code().unwrap_or(-1);

    let mut result = String::new();
    if !stdout.is_empty() {
        result.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&format!("[stderr]\n{stderr}"));
    }
    result.push_str(&format!("\n[exit: {exit}]"));
    result
}

fn compile_success(output: &str) -> bool {
    output.trim_end().ends_with("[exit: 0]")
}

fn temp_run_dir(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{}_{}_{}",
        prefix,
        std::process::id(),
        rand::random::<u64>()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sandboxed_python_command_runs_when_bwrap_exists() {
        if !Path::new("/usr/bin/bwrap").exists() || !Path::new("/usr/bin/python3").exists() {
            return;
        }

        let dir = temp_run_dir("uintell_code_test");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let output = run_command("python3", &["-c", "print('sandbox-ok')"], Some(&dir), 5)
            .await
            .unwrap();
        let _ = tokio::fs::remove_dir_all(&dir).await;

        assert!(output.contains("sandbox-ok"), "{output}");
        assert!(output.trim_end().ends_with("[exit: 0]"), "{output}");
    }

    #[tokio::test]
    async fn rust_code_compiles_and_runs_in_sandbox() {
        if !Path::new("/usr/bin/bwrap").exists() || resolve_rustc().is_err() {
            return;
        }

        let output = exec_rust(r#"fn main() { println!("rust-sandbox-ok"); }"#, 10)
            .await
            .unwrap();

        assert!(output.contains("rust-sandbox-ok"), "{output}");
        assert!(output.trim_end().ends_with("[exit: 0]"), "{output}");
    }

    #[test]
    fn sandbox_is_enabled_by_default() {
        std::env::remove_var("UINTELL_CODE_SANDBOX");
        assert!(sandbox_enabled());
    }
}
