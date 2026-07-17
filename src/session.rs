// Persistent Terminal Session — long-lived bash with state persistence
//
// Uses temp-file redirection per command. Bash process stays alive,
// preserving cd, exports, venv, background jobs.
//
// Exit codes are captured via `echo $?` redirect, not approximated.
// No sleep polling — uses file size stabilization detection.

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

static SESSION: std::sync::LazyLock<Arc<Mutex<Option<Session>>>> =
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(None)));

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
        // Graceful: send exit, wait 2s, then kill
        let _ = sess.stdin.write_all(b"exit\n").await;
        let _ = sess.stdin.flush().await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = sess.child.start_kill();
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
}

impl Session {
    fn spawn() -> std::io::Result<Self> {
        let mut command = Command::new("bash");
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("no stdin"))?;
        Ok(Self {
            child,
            stdin,
            cwd: None,
            cmd_count: 0,
        })
    }

    async fn exec(&mut self, command: &str, timeout_secs: u64) -> Result<ExecResult, ExecError> {
        let start = Instant::now();
        self.cmd_count += 1;
        let id = self.cmd_count;
        let pid = std::process::id();
        let out_file = format!("/tmp/uintell_out_{pid}_{id}.txt");
        let err_file = format!("/tmp/uintell_err_{pid}_{id}.txt");
        let ec_file = format!("/tmp/uintell_ec_{pid}_{id}.txt");
        let cwd_file = format!("/tmp/uintell_cwd_{pid}_{id}.txt");

        // Braces execute in the persistent shell, so `cd` and exports survive.
        // The EXIT trap still records a result if the command exits the shell.
        let full_cmd = format!(
            "trap 'ec=$?; echo $ec > \"{ec_file}\"; pwd > \"{cwd_file}\"' EXIT\n\
             {{\n{command}\n}} > '{out_file}' 2> '{err_file}'\n\
             ec=$?\n\
             trap - EXIT\n\
             echo $ec > '{ec_file}'\n\
             pwd > '{cwd_file}'\n"
        );
        self.stdin.write_all(full_cmd.as_bytes()).await?;
        self.stdin.flush().await?;

        // Wait for exit code file to appear (signals command completion)
        let wait_future = async {
            loop {
                if tokio::fs::metadata(&ec_file).await.is_ok() {
                    // File exists — command is done. Small delay for flush.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };

        match timeout(Duration::from_secs(timeout_secs), wait_future).await {
            Ok(()) => {}
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
                *self = Session::spawn()?;
                cleanup(&out_file, &err_file, &ec_file, &cwd_file).await;
                return Err(ExecError::Timeout(timeout_secs));
            }
        }

        // Read results
        let stdout = read_limited(&out_file, 50_000).await;
        let stderr = read_limited(&err_file, 10_000).await;
        let exit_code = tokio::fs::read_to_string(&ec_file)
            .await
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(-1);
        let cwd = tokio::fs::read_to_string(&cwd_file)
            .await
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "/".into());

        self.cwd = Some(cwd.clone());
        cleanup(&out_file, &err_file, &ec_file, &cwd_file).await;

        if self.child.try_wait()?.is_some() {
            *self = Session::spawn()?;
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

async fn read_limited(path: &str, max_bytes: usize) -> String {
    match tokio::fs::read_to_string(path).await {
        Ok(s) if s.len() > max_bytes => format!("{}... ({}B)", &s[..max_bytes], s.len()),
        Ok(s) => s,
        Err(_) => String::new(),
    }
}

async fn cleanup(out: &str, err: &str, ec: &str, cwd: &str) {
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
            let _ = sess.child.start_kill();
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
}
