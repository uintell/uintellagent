// Persistent Terminal Session — long-lived bash with state persistence
//
// Uses temp-file redirection per command. Bash process stays alive,
// preserving cd, exports, venv, background jobs.
//
// Exit codes are captured via `echo $?` redirect, not approximated.
// No sleep polling — uses file size stabilization detection.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

static SESSION: std::sync::LazyLock<Arc<Mutex<Option<Session>>>> =
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(None)));
const MAX_STDOUT_BYTES: usize = 50_000;
const MAX_STDERR_BYTES: usize = 10_000;

pub async fn get_or_create_session() -> anyhow::Result<()> {
    let mut s = SESSION.lock().await;
    if s.is_none() {
        *s = Some(Session::spawn()?);
    }
    Ok(())
}

pub async fn exec(command: &str, timeout_secs: u64) -> Result<ExecResult, ExecError> {
    let mut guard = SESSION.lock().await;
    let session = guard.as_mut().ok_or(ExecError::NoSession)?;
    session.exec(command, timeout_secs).await
}

pub async fn get_cwd() -> Option<String> {
    SESSION.lock().await.as_ref().and_then(|s| s.cwd.clone())
}

pub async fn kill_session() {
    let mut s = SESSION.lock().await;
    if let Some(mut sess) = s.take() {
        let temp_dir = sess.temp_dir.clone();
        // Graceful: send exit, wait 2s, then kill.
        let _ = sess.stdin.write_all(b"exit\n").await;
        let _ = sess.stdin.flush().await;
        if !matches!(
            tokio::time::timeout(Duration::from_secs(2), sess.child.wait()).await,
            Ok(Ok(_))
        ) {
            let _ = sess.child.start_kill();
            let _ = sess.child.wait().await;
        }
        let _ = tokio::fs::remove_dir_all(temp_dir).await;
    }
}

pub async fn restart_session() -> anyhow::Result<()> {
    kill_session().await;
    get_or_create_session().await
}

#[derive(Debug)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub elapsed: Duration,
    pub cwd: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("no active session")]
    NoSession,
    #[error("timeout after {0}s")]
    Timeout(u64),
    #[error("session died: {0}")]
    #[allow(dead_code)]
    SessionDied(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

struct Session {
    child: Child,
    stdin: tokio::process::ChildStdin,
    cwd: Option<String>,
    cmd_count: u64,
    temp_dir: PathBuf,
}

impl Session {
    fn spawn() -> std::io::Result<Self> {
        let temp_dir = create_session_temp_dir()?;
        let mut command = Command::new("bash");
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&temp_dir);
                return Err(error);
            }
        };
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                let _ = child.start_kill();
                let _ = std::fs::remove_dir_all(&temp_dir);
                return Err(std::io::Error::other("no stdin"));
            }
        };
        Ok(Self {
            child,
            stdin,
            cwd: None,
            cmd_count: 0,
            temp_dir,
        })
    }

    async fn exec(&mut self, command: &str, timeout_secs: u64) -> Result<ExecResult, ExecError> {
        let start = Instant::now();
        self.cmd_count += 1;
        let id = self.cmd_count;
        let out_file = self.temp_dir.join(format!("out-{id}.txt"));
        let err_file = self.temp_dir.join(format!("err-{id}.txt"));
        let ec_file = self.temp_dir.join(format!("exit-{id}.txt"));
        let cwd_file = self.temp_dir.join(format!("cwd-{id}.txt"));
        let out_file_quoted = shell_quote(&out_file.to_string_lossy());
        let err_file_quoted = shell_quote(&err_file.to_string_lossy());
        let ec_file_quoted = shell_quote(&ec_file.to_string_lossy());
        let cwd_file_quoted = shell_quote(&cwd_file.to_string_lossy());
        let exit_trap = shell_quote(&format!(
            "ec=$?; exec 8>&- 9>&-; builtin printf '%s\\n' \"$ec\" > {ec_file_quoted}; builtin pwd > {cwd_file_quoted}"
        ));

        // Braces execute in the persistent shell, so `cd` and exports survive.
        // Process substitutions retain a bounded prefix and drain the remainder.
        let full_cmd = format!(
            "builtin pwd > {cwd_file_quoted}\n\
             exec 8> >({{ exec 8>&- 9>&-; command -p head -c {} > {out_file_quoted}; command -p cat >/dev/null; }})\n\
             uintell_out_pid=$!\n\
             exec 9> >({{ exec 8>&- 9>&-; command -p head -c {} > {err_file_quoted}; command -p cat >/dev/null; }})\n\
             uintell_err_pid=$!\n\
             builtin trap {exit_trap} EXIT\n\
             {{\n{command}\n}} >&8 2>&9\n\
             ec=$?\n\
             exec 8>&- 9>&-\n\
             builtin wait \"$uintell_out_pid\" \"$uintell_err_pid\" 2>/dev/null || true\n\
             builtin trap - EXIT\n\
             builtin printf '%s\\n' \"$ec\" > {ec_file_quoted}\n\
             builtin pwd > {cwd_file_quoted}\n",
            MAX_STDOUT_BYTES + 1,
            MAX_STDERR_BYTES + 1,
        );
        self.stdin.write_all(full_cmd.as_bytes()).await?;
        self.stdin.flush().await?;

        // Wait for exit code file to appear (signals command completion)
        let wait_future = async {
            loop {
                if tokio::fs::metadata(&ec_file).await.is_ok() {
                    // File exists — command is done. Small delay for flush.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    return Ok::<Option<i32>, std::io::Error>(None);
                }
                if let Some(status) = self.child.try_wait()? {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    return Ok(Some(exit_status_code(status)));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };

        let shell_exit_code = match timeout(Duration::from_secs(timeout_secs), wait_future).await {
            Ok(Ok(code)) => code,
            Ok(Err(error)) => return Err(ExecError::Io(error)),
            Err(_) => {
                #[cfg(unix)]
                if let Some(pid) = self.child.id() {
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGKILL);
                    }
                }
                #[cfg(not(unix))]
                let _ = self.child.start_kill();
                let _ = self.child.wait().await;
                let old_temp_dir = self.temp_dir.clone();
                *self = Session::spawn()?;
                let _ = tokio::fs::remove_dir_all(old_temp_dir).await;
                return Err(ExecError::Timeout(timeout_secs));
            }
        };

        // Read results
        let stdout = read_limited(&out_file, MAX_STDOUT_BYTES).await;
        let stderr = read_limited(&err_file, MAX_STDERR_BYTES).await;
        let exit_code = tokio::fs::read_to_string(&ec_file)
            .await
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .or(shell_exit_code)
            .unwrap_or(-1);
        let cwd = tokio::fs::read_to_string(&cwd_file)
            .await
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "/".into());

        self.cwd = Some(cwd.clone());
        cleanup(&out_file, &err_file, &ec_file, &cwd_file).await;

        if shell_exit_code.is_some() || self.child.try_wait()?.is_some() {
            let old_temp_dir = self.temp_dir.clone();
            *self = Session::spawn()?;
            let _ = tokio::fs::remove_dir_all(old_temp_dir).await;
        }

        Ok(ExecResult {
            stdout,
            stderr,
            exit_code,
            elapsed: start.elapsed(),
            cwd,
        })
    }
}

fn exit_status_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map_or(-1, |signal| 128 + signal)
    }
    #[cfg(not(unix))]
    -1
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

fn create_session_temp_dir() -> std::io::Result<PathBuf> {
    for _ in 0..16 {
        let path = std::env::temp_dir().join(format!(
            "uintell-session-{}-{:x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a private session directory",
    ))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

async fn read_limited(path: &Path, max_bytes: usize) -> String {
    let Ok(file) = tokio::fs::File::open(path).await else {
        return String::new();
    };
    let total_bytes = file
        .metadata()
        .await
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    let mut bytes = Vec::with_capacity(max_bytes.min(total_bytes as usize));
    if file
        .take(max_bytes as u64)
        .read_to_end(&mut bytes)
        .await
        .is_err()
    {
        return String::new();
    }
    let output = String::from_utf8_lossy(&bytes);
    if total_bytes > max_bytes as u64 {
        format!("{output}... (truncated at {max_bytes} bytes; {total_bytes} bytes total)")
    } else {
        output.into_owned()
    }
}

async fn cleanup(out: &Path, err: &Path, ec: &Path, cwd: &Path) {
    let _ = tokio::fs::remove_file(out).await;
    let _ = tokio::fs::remove_file(err).await;
    let _ = tokio::fs::remove_file(ec).await;
    let _ = tokio::fs::remove_file(cwd).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn setup() {
        // Force-clear any stale session from previous test
        let mut s = SESSION.lock().await;
        if let Some(mut sess) = s.take() {
            let temp_dir = sess.temp_dir.clone();
            let _ = sess.child.start_kill();
            let _ = tokio::fs::remove_dir_all(temp_dir).await;
        }
        // Small delay for OS cleanup
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Clear temp files
        let pid = std::process::id();
        for i in 1..10 {
            let _ = tokio::fs::remove_file(format!("/tmp/uintell_out_{pid}_{i}.txt")).await;
            let _ = tokio::fs::remove_file(format!("/tmp/uintell_err_{pid}_{i}.txt")).await;
            let _ = tokio::fs::remove_file(format!("/tmp/uintell_ec_{pid}_{i}.txt")).await;
            let _ = tokio::fs::remove_file(format!("/tmp/uintell_cwd_{pid}_{i}.txt")).await;
        }
    }

    #[tokio::test]
    async fn test_session_basic() {
        let _guard = TEST_LOCK.lock().await;
        setup().await;
        get_or_create_session().await.expect("create session");
        let r = exec("echo hello", 5).await.expect("exec echo");
        assert!(r.stdout.contains("hello"));
        assert_eq!(r.exit_code, 0);
    }

    #[tokio::test]
    async fn test_session_cd_persists() {
        let _guard = TEST_LOCK.lock().await;
        setup().await;
        get_or_create_session().await.expect("create session");
        exec("cd /tmp", 5).await.expect("cd");
        let r = exec("pwd", 5).await.expect("pwd");
        assert!(
            r.stdout.contains("/tmp"),
            "expected /tmp, got: {}",
            r.stdout
        );
    }

    #[tokio::test]
    async fn test_session_exit_code() {
        let _guard = TEST_LOCK.lock().await;
        setup().await;
        get_or_create_session().await.expect("create session");
        let r = exec("exit 42", 5).await.expect("exit 42");
        assert_eq!(r.exit_code, 42, "expected 42, got {}", r.exit_code);
    }

    #[tokio::test]
    async fn exec_replacing_the_shell_completes_and_restarts_the_session() {
        let _guard = TEST_LOCK.lock().await;
        setup().await;
        get_or_create_session().await.expect("create session");
        let replaced = exec("exec true", 5).await.expect("exec true");
        assert_eq!(replaced.exit_code, 0);
        let next = exec("printf restarted", 5)
            .await
            .expect("replacement session");
        assert_eq!(next.stdout, "restarted");
    }

    #[tokio::test]
    async fn test_session_timeout() {
        let _guard = TEST_LOCK.lock().await;
        setup().await;
        get_or_create_session().await.expect("create session");
        let r = exec("sleep 10", 1).await;
        assert!(
            matches!(r, Err(ExecError::Timeout(1))),
            "expected timeout, got: {r:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn session_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let path = create_session_temp_dir().unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
        std::fs::remove_dir(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn session_paths_are_safe_for_shell_interpolation() {
        let value = "/tmp/uintell's output";
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!("printf %s {}", shell_quote(value)))
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), value);
    }

    #[tokio::test]
    async fn output_truncation_is_safe_for_unicode_and_binary_data() {
        let path = std::env::temp_dir().join(format!(
            "uintell-output-{}-{:x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        tokio::fs::write(&path, "ééé".as_bytes()).await.unwrap();
        let output = read_limited(&path, 3).await;
        assert!(output.contains("truncated at 3 bytes"));
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn capture_limits_do_not_limit_files_written_by_commands() {
        let _guard = TEST_LOCK.lock().await;
        setup().await;
        get_or_create_session().await.expect("create session");
        let path = std::env::temp_dir().join(format!(
            "uintell-large-write-{}-{:x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let command = format!(
            "head -c 1000000 /dev/zero > {}; head -c 60000 /dev/zero; head -c 20000 /dev/zero >&2",
            shell_quote(&path.to_string_lossy())
        );
        let result = exec(&command, 5).await.expect("large output command");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1_000_000);
        assert!(result.stdout.contains("truncated at 50000 bytes"));
        assert!(result.stderr.contains("truncated at 10000 bytes"));
        std::fs::remove_file(path).unwrap();
    }
}
