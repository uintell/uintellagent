// Permission System — allow/deny lists, modes, confirmation
//
// Config: ~/.uintell/permissions.toml
//
// Modes:
//   read-only     — no file writes, no shell, no network writes, no DB writes
//   workspace     — write only within workspace dirs, shell with allow-list
//   full-access   — everything allowed, destructive ops still confirmed in TUI

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{LazyLock, Mutex};

static APPROVED_CALLS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionsConfig {
    pub mode: PermissionMode,
    /// Directories where writes are allowed (workspace mode)
    #[serde(default)]
    pub workspace_dirs: Vec<String>,
    /// Shell commands allowed (regex patterns, workspace + full mode)
    #[serde(default)]
    pub allowed_commands: Vec<String>,
    /// Shell commands always denied
    #[serde(default)]
    pub denied_commands: Vec<String>,
    /// File paths allowed for read (glob patterns)
    #[serde(default)]
    pub allowed_read_paths: Vec<String>,
    /// File paths denied for read
    #[serde(default)]
    pub denied_read_paths: Vec<String>,
    /// File paths allowed for write (glob patterns)
    #[serde(default)]
    pub allowed_write_paths: Vec<String>,
    /// File paths denied for write
    #[serde(default)]
    pub denied_write_paths: Vec<String>,
    /// Network hosts allowed
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Network hosts denied
    #[serde(default)]
    pub denied_hosts: Vec<String>,
    /// Require confirmation for destructive ops even in full-access
    #[serde(default = "default_true")]
    pub confirm_destructive: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    ReadOnly,
    Workspace,
    FullAccess,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            mode: PermissionMode::Workspace,
            workspace_dirs: vec![".".into(), "/Uintellagent".into()],
            allowed_commands: vec![
                "ls".into(),
                "cat".into(),
                "head".into(),
                "tail".into(),
                "grep".into(),
                "rg".into(),
                "find".into(),
                "file".into(),
                "git".into(),
                "cargo".into(),
                "python".into(),
                "python3".into(),
                "node".into(),
                "rustc".into(),
                "code_exec".into(),
                "npm".into(),
                "pnpm".into(),
                "curl".into(),
                "wget".into(),
                "systemctl".into(),
                "docker".into(),
                "ps".into(),
                "top".into(),
                "htop".into(),
                "df".into(),
                "du".into(),
                "echo".into(),
                "printf".into(),
                "pwd".into(),
                "cd".into(),
                "pushd".into(),
                "popd".into(),
                "sed".into(),
                "awk".into(),
                "wc".into(),
                "which".into(),
                "touch".into(),
                "env".into(),
                "mkdir".into(),
                "cp".into(),
                "mv".into(),
                "rm".into(),
                "chmod".into(),
                "chown".into(),
                "cargo build".into(),
                "cargo test".into(),
                "cargo run".into(),
                "cargo check".into(),
                "cargo clippy".into(),
                "cargo fmt".into(),
                "git add".into(),
                "git commit".into(),
                "git push".into(),
                "git pull".into(),
                "git status".into(),
                "git diff".into(),
                "git log".into(),
                "systemctl start".into(),
                "systemctl stop".into(),
                "systemctl restart".into(),
                "systemctl status".into(),
            ],
            denied_commands: vec![
                "rm -rf /".into(),
                "dd if=".into(),
                "mkfs".into(),
                ":(){ :|:& };:".into(),
                "shutdown".into(),
                "reboot".into(),
                "halt".into(),
                "poweroff".into(),
            ],
            allowed_read_paths: vec![
                "~".into(),
                "/home".into(),
                "/tmp".into(),
                "/Uintellagent".into(),
            ],
            denied_read_paths: vec![
                "/etc/shadow".into(),
                ".ssh".into(),
                "*.pem".into(),
                "*.key".into(),
                "id_rsa".into(),
                "id_ed25519".into(),
            ],
            allowed_write_paths: vec![
                "/tmp".into(),
                "/Uintellagent".into(),
                "/home/x1".into(),
                ".".into(),
            ],
            denied_write_paths: vec![
                "/etc".into(),
                "/boot".into(),
                "/sys".into(),
                "/proc".into(),
                "/dev".into(),
            ],
            allowed_hosts: vec![
                "api.deepseek.com".into(),
                "127.0.0.1".into(),
                "localhost".into(),
                "duckduckgo.com".into(),
                "html.duckduckgo.com".into(),
                "crates.io".into(),
                "github.com".into(),
                "raw.githubusercontent.com".into(),
            ],
            denied_hosts: vec![],
            confirm_destructive: true,
        }
    }
}

impl PermissionsConfig {
    pub fn load() -> Self {
        let path = config_path();
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(cfg) = toml::from_str(&content) {
                    return cfg;
                }
            }
        }
        // Write default config
        let default = Self::default();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(toml_str) = toml::to_string_pretty(&default) {
            let _ = std::fs::write(&path, toml_str);
        }
        default
    }

    pub fn can_execute_shell(&self, command: &str) -> PermissionResult {
        if self.mode == PermissionMode::ReadOnly {
            return PermissionResult::Denied("read-only mode: no shell access".into());
        }
        // Check denied first
        for pattern in &self.denied_commands {
            if denied_command_matches(pattern, command) {
                return PermissionResult::Denied(format!(
                    "command matches deny pattern: {pattern}"
                ));
            }
        }
        if self.confirm_destructive && is_destructive_command(command) {
            return PermissionResult::Confirm(format!(
                "destructive shell command requires confirmation: {command}"
            ));
        }
        // In full access, allow anything not denied
        if self.mode == PermissionMode::FullAccess {
            return PermissionResult::Allowed;
        }
        // Workspace mode: check allow list
        for pattern in &self.allowed_commands {
            if command.starts_with(pattern.as_str()) {
                return PermissionResult::Allowed;
            }
        }
        PermissionResult::Confirm(format!("command not in allow-list: {command}"))
    }

    pub fn can_read_file(&self, path: &Path) -> PermissionResult {
        let normalized = normalize_path(path);
        let path_str = normalized.to_string_lossy();
        // Check denied first
        for pattern in &self.denied_read_paths {
            if glob_match(pattern, &path_str) {
                return PermissionResult::Denied(format!("path matches deny pattern: {pattern}"));
            }
        }
        if self.mode == PermissionMode::FullAccess {
            return PermissionResult::Allowed;
        }
        for pattern in &self.allowed_read_paths {
            if glob_match(pattern, &path_str) {
                return PermissionResult::Allowed;
            }
        }
        // Workspace dirs
        if self.mode == PermissionMode::Workspace {
            for dir in &self.workspace_dirs {
                if path_within_dir(&normalized, dir) {
                    return PermissionResult::Allowed;
                }
            }
        }
        PermissionResult::Denied(format!("read not allowed: {path_str}"))
    }

    pub fn can_write_file(&self, path: &Path) -> PermissionResult {
        if self.mode == PermissionMode::ReadOnly {
            return PermissionResult::Denied("read-only mode: no file writes".into());
        }
        let normalized = normalize_path(path);
        let path_str = normalized.to_string_lossy();
        for pattern in &self.denied_write_paths {
            if glob_match(pattern, &path_str) {
                return PermissionResult::Denied(format!("path matches deny pattern: {pattern}"));
            }
        }
        if self.mode == PermissionMode::FullAccess {
            return PermissionResult::Allowed;
        }
        for pattern in &self.allowed_write_paths {
            if glob_match(pattern, &path_str) {
                return PermissionResult::Allowed;
            }
        }
        for dir in &self.workspace_dirs {
            if path_within_dir(&normalized, dir) {
                return PermissionResult::Allowed;
            }
        }
        PermissionResult::Confirm(format!("write not in allow-list: {path_str}"))
    }

    pub fn can_access_network(&self, host: &str) -> PermissionResult {
        for pattern in &self.denied_hosts {
            if host.contains(pattern.as_str()) {
                return PermissionResult::Denied(format!("host matches deny pattern: {pattern}"));
            }
        }
        if self.mode == PermissionMode::FullAccess {
            return PermissionResult::Allowed;
        }
        for pattern in &self.allowed_hosts {
            if host.contains(pattern.as_str()) || pattern.contains(host) {
                return PermissionResult::Allowed;
            }
        }
        PermissionResult::Confirm(format!("host not in allow-list: {host}"))
    }

    pub fn can_access_db(&self, operation: &str) -> PermissionResult {
        if self.mode == PermissionMode::ReadOnly
            && !operation.starts_with("SELECT")
            && !operation.starts_with("RETURN")
            && !operation.starts_with("INFO")
        {
            return PermissionResult::Denied("read-only mode: DB writes not allowed".into());
        }
        if self.confirm_destructive && matches!(operation, "DELETE" | "DROP" | "REMOVE") {
            return PermissionResult::Confirm(format!(
                "destructive DB operation requires confirmation: {operation}"
            ));
        }
        PermissionResult::Allowed
    }
}

#[derive(Debug, Clone)]
pub enum PermissionResult {
    Allowed,
    Denied(String),
    Confirm(String),
}

pub fn permission_for_tool(tool_name: &str, args_json: &str) -> PermissionResult {
    let cfg = PermissionsConfig::load();
    let args = serde_json::from_str::<Value>(args_json).unwrap_or(Value::Null);

    match tool_name {
        "terminal" => string_arg(&args, "command")
            .map(|command| cfg.can_execute_shell(command))
            .unwrap_or_else(|| PermissionResult::Denied("missing terminal command".into())),
        "code_exec" => cfg.can_execute_shell("code_exec"),
        "file_read" => string_arg(&args, "path")
            .map(|path| cfg.can_read_file(Path::new(path)))
            .unwrap_or_else(|| PermissionResult::Denied("missing file path".into())),
        "file_write" => string_arg(&args, "path")
            .map(|path| cfg.can_write_file(Path::new(path)))
            .unwrap_or_else(|| PermissionResult::Denied("missing file path".into())),
        "file_search" => {
            let path = string_arg(&args, "path").unwrap_or(".");
            cfg.can_read_file(Path::new(path))
        }
        "browser" => string_arg(&args, "url")
            .and_then(url_host)
            .map(|host| cfg.can_access_network(&host))
            .unwrap_or_else(|| PermissionResult::Denied("missing or invalid URL".into())),
        "web_search" => cfg.can_access_network("duckduckgo.com"),
        "graph_store" => cfg.can_access_db("CREATE"),
        "graph_edit" => cfg.can_access_db("UPDATE"),
        "graph_forget" => cfg.can_access_db("DELETE"),
        "graph_query" | "graph_context" => cfg.can_access_db("SELECT"),
        _ => PermissionResult::Allowed,
    }
}

pub fn record_approval(tool_name: &str, args_json: &str) {
    if let Ok(mut approved) = APPROVED_CALLS.lock() {
        approved.insert(approval_key(tool_name, args_json));
    }
}

pub fn enforce_tool_call(tool_name: &str, args_json: &str) -> Result<(), String> {
    match permission_for_tool(tool_name, args_json) {
        PermissionResult::Allowed => Ok(()),
        PermissionResult::Denied(reason) => Err(format!("PERMISSION DENIED: {reason}")),
        PermissionResult::Confirm(reason) => {
            let key = approval_key(tool_name, args_json);
            let approved = APPROVED_CALLS
                .lock()
                .map(|mut approvals| approvals.remove(&key))
                .unwrap_or(false);
            if approved {
                Ok(())
            } else {
                Err(format!("CONFIRMATION REQUIRED: {reason}"))
            }
        }
    }
}

pub(crate) fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".uintell")
        .join("permissions.toml")
}

fn expand_tilde(path: &str) -> String {
    if path.starts_with('~') {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        path.replacen('~', &home, 1)
    } else {
        path.to_string()
    }
}

fn string_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn url_host(url: &str) -> Option<String> {
    let s = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    Some(s.split('/').next()?.split(':').next()?.to_string())
}

fn approval_key(tool_name: &str, args_json: &str) -> String {
    let args = serde_json::from_str::<Value>(args_json).unwrap_or(Value::Null);
    let subject = match tool_name {
        "terminal" => string_arg(&args, "command").unwrap_or("").to_string(),
        "code_exec" => format!(
            "{}:{}",
            string_arg(&args, "language").unwrap_or("python"),
            string_arg(&args, "code").unwrap_or("")
        ),
        "file_read" | "file_write" | "file_search" => {
            string_arg(&args, "path").unwrap_or(".").to_string()
        }
        "browser" => string_arg(&args, "url").unwrap_or("").to_string(),
        "web_search" => string_arg(&args, "query").unwrap_or("").to_string(),
        "graph_edit" | "graph_forget" => string_arg(&args, "fact_id").unwrap_or("").to_string(),
        _ => serde_json::to_string(&args).unwrap_or_else(|_| args_json.to_string()),
    };
    format!("{tool_name}:{}", blake3::hash(subject.as_bytes()))
}

fn is_destructive_command(command: &str) -> bool {
    let trimmed = command.trim_start();
    let first = trimmed.split_whitespace().next().unwrap_or("");
    matches!(
        first,
        "rm" | "mv"
            | "chmod"
            | "chown"
            | "dd"
            | "mkfs"
            | "shutdown"
            | "reboot"
            | "halt"
            | "poweroff"
    ) || trimmed.contains(" rm -")
        || trimmed.contains(">/dev/")
}

fn denied_command_matches(pattern: &str, command: &str) -> bool {
    let command = command.trim();
    if pattern == "rm -rf /" {
        return matches!(command, "rm -rf /" | "sudo rm -rf /");
    }
    command.contains(pattern)
}

fn normalize_path(path: &Path) -> PathBuf {
    let expanded = expand_tilde(&path.to_string_lossy());
    let mut path = PathBuf::from(expanded);
    if path.is_relative() {
        path = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path);
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn path_within_dir(path: &Path, dir: &str) -> bool {
    let base = normalize_path(Path::new(dir));
    path.starts_with(base)
}

/// Simple glob matching: * matches anything, otherwise exact
fn glob_match(pattern: &str, s: &str) -> bool {
    let pattern = expand_tilde(pattern);
    let pattern = pattern.as_str();
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return s.contains(pattern) || expand_tilde(pattern) == s;
    }
    // Basic glob: prefix* or *suffix or prefix*suffix
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 2 {
        if pattern.starts_with('*') {
            return s.ends_with(parts[1]);
        }
        if pattern.ends_with('*') {
            return s.starts_with(parts[0]);
        }
        return s.starts_with(parts[0]) && s.ends_with(parts[1]);
    }
    s.contains(pattern.trim_matches('*'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_paths_are_denied_before_broad_home_allow() {
        let cfg = PermissionsConfig::default();
        let result = cfg.can_read_file(Path::new("/home/x1/.ssh/id_ed25519"));
        assert!(matches!(result, PermissionResult::Denied(_)));
    }

    #[test]
    fn destructive_shell_commands_require_confirmation() {
        let cfg = PermissionsConfig::default();
        let result = cfg.can_execute_shell("rm -rf /tmp/uintell-test");
        assert!(matches!(result, PermissionResult::Confirm(_)));
    }

    #[test]
    fn denied_shell_patterns_win_over_confirmation() {
        let cfg = PermissionsConfig::default();
        let result = cfg.can_execute_shell("rm -rf /");
        assert!(matches!(result, PermissionResult::Denied(_)));
    }

    #[test]
    fn workspace_paths_are_normalized_before_matching() {
        let cfg = PermissionsConfig {
            workspace_dirs: vec!["/Uintellagent".into()],
            allowed_write_paths: vec![],
            ..PermissionsConfig::default()
        };
        let result = cfg.can_write_file(Path::new("/Uintellagent/src/../Cargo.toml"));
        assert!(matches!(result, PermissionResult::Allowed));
    }
}
