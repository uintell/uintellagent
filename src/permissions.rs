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
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{LazyLock, Mutex};

static APPROVED_CALLS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
const PERMISSIONS_CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionsConfig {
    #[serde(default = "current_config_version")]
    pub config_version: u32,
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

fn current_config_version() -> u32 {
    PERMISSIONS_CONFIG_VERSION
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
            config_version: PERMISSIONS_CONFIG_VERSION,
            mode: PermissionMode::Workspace,
            workspace_dirs: vec![".".into()],
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
                "code_exec".into(),
                "npm".into(),
                "pnpm".into(),
                "ps".into(),
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
            allowed_read_paths: vec!["/tmp".into()],
            denied_read_paths: vec![
                "/etc/shadow".into(),
                ".ssh".into(),
                ".aws".into(),
                ".gnupg".into(),
                ".kube".into(),
                ".docker".into(),
                ".env*".into(),
                ".netrc".into(),
                ".npmrc".into(),
                ".pypirc".into(),
                "*.pem".into(),
                "*.key".into(),
                "*.p12".into(),
                "*.pfx".into(),
                "id_rsa".into(),
                "id_ed25519".into(),
            ],
            allowed_write_paths: vec!["/tmp".into()],
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
                "openrouter.ai".into(),
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
            match std::fs::read_to_string(&path).map_err(|error| error.to_string()) {
                Ok(content) => match toml::from_str::<Self>(&content) {
                    Ok(mut cfg) => {
                        let has_version = toml::from_str::<toml::Value>(&content)
                            .ok()
                            .and_then(|value| value.as_table().cloned())
                            .is_some_and(|table| table.contains_key("config_version"));
                        if !has_version {
                            cfg.migrate_alpha_defaults();
                            if let Err(error) = write_default_config(&path, &cfg) {
                                eprintln!(
                                    "UIntell could not persist the permission migration at {}: {error}",
                                    path.display()
                                );
                            }
                        }
                        if let Err(error) = cfg.validate() {
                            eprintln!(
                            "UIntell permissions are invalid at {}: {error}; using read-only policy",
                            path.display()
                        );
                            return Self::fail_closed();
                        }
                        return cfg;
                    }
                    Err(error) => {
                        eprintln!(
                            "UIntell permissions could not be loaded from {}: {error}; using read-only policy",
                            path.display()
                        );
                        return Self::fail_closed();
                    }
                },
                Err(error) => {
                    eprintln!(
                        "UIntell permissions could not be loaded from {}: {error}; using read-only policy",
                        path.display()
                    );
                    return Self::fail_closed();
                }
            }
        }

        let default = Self::default();
        if let Err(error) = write_default_config(&path, &default) {
            eprintln!(
                "UIntell could not create {}: {error}; using read-only policy",
                path.display()
            );
            return Self::fail_closed();
        }
        default
    }

    fn migrate_alpha_defaults(&mut self) {
        self.config_version = PERMISSIONS_CONFIG_VERSION;
        self.workspace_dirs.retain(|path| path != "/Uintellagent");
        if self.workspace_dirs.is_empty() {
            self.workspace_dirs.push(".".into());
        }
        self.allowed_read_paths
            .retain(|path| !matches!(path.as_str(), "~" | "/home" | "/Uintellagent"));
        self.allowed_write_paths
            .retain(|path| !matches!(path.as_str(), "/Uintellagent" | "/home/x1" | "."));

        self.allowed_commands.retain(|command| {
            !matches!(
                command.as_str(),
                "python"
                    | "python3"
                    | "node"
                    | "rustc"
                    | "curl"
                    | "wget"
                    | "systemctl"
                    | "docker"
                    | "top"
                    | "htop"
                    | "touch"
                    | "env"
                    | "mkdir"
                    | "cp"
                    | "mv"
                    | "rm"
                    | "chmod"
                    | "chown"
            )
        });
        for pattern in Self::default().denied_read_paths {
            if !self.denied_read_paths.contains(&pattern) {
                self.denied_read_paths.push(pattern);
            }
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.config_version != PERMISSIONS_CONFIG_VERSION {
            return Err(format!(
                "unsupported config version {} (expected {})",
                self.config_version, PERMISSIONS_CONFIG_VERSION
            ));
        }
        Ok(())
    }

    fn fail_closed() -> Self {
        Self {
            config_version: PERMISSIONS_CONFIG_VERSION,
            mode: PermissionMode::ReadOnly,
            workspace_dirs: Vec::new(),
            allowed_commands: Vec::new(),
            denied_commands: vec!["*".into()],
            allowed_read_paths: Vec::new(),
            denied_read_paths: vec!["*".into()],
            allowed_write_paths: Vec::new(),
            denied_write_paths: vec!["*".into()],
            allowed_hosts: Vec::new(),
            denied_hosts: vec!["*".into()],
            confirm_destructive: true,
        }
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
        if has_unquoted_shell_control(command) {
            return PermissionResult::Confirm(
                "shell chaining, substitution, or redirection requires confirmation".into(),
            );
        }
        if self.confirm_destructive && requires_shell_confirmation(command) {
            return PermissionResult::Confirm(format!(
                "state-changing or privileged shell command requires confirmation: {command}"
            ));
        }
        // In full access, allow anything not denied
        if self.mode == PermissionMode::FullAccess {
            return PermissionResult::Allowed;
        }
        // Workspace mode: check allow list
        for pattern in &self.allowed_commands {
            if command_pattern_matches(pattern, command) {
                return PermissionResult::Allowed;
            }
        }
        PermissionResult::Confirm(format!("command not in allow-list: {command}"))
    }

    pub fn can_read_file(&self, path: &Path) -> PermissionResult {
        let normalized = resolve_for_policy(path);
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
        let normalized = resolve_for_policy(path);
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
            if domain_matches(pattern, host) {
                return PermissionResult::Denied(format!("host matches deny pattern: {pattern}"));
            }
        }
        if self.mode == PermissionMode::FullAccess {
            return PermissionResult::Allowed;
        }
        for pattern in &self.allowed_hosts {
            if domain_matches(pattern, host) {
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
    permission_for_tool_with_config(&cfg, tool_name, &args)
}

fn permission_for_tool_with_config(
    cfg: &PermissionsConfig,
    tool_name: &str,
    args: &Value,
) -> PermissionResult {
    match tool_name {
        "terminal" => string_arg(args, "command")
            .map(|command| cfg.can_execute_shell(command))
            .unwrap_or_else(|| PermissionResult::Denied("missing terminal command".into())),
        "code_exec" => cfg.can_execute_shell("code_exec"),
        "file_read" => string_arg(args, "path")
            .map(|path| cfg.can_read_file(Path::new(path)))
            .unwrap_or_else(|| PermissionResult::Denied("missing file path".into())),
        "file_write" => string_arg(args, "path")
            .map(|path| cfg.can_write_file(Path::new(path)))
            .unwrap_or_else(|| PermissionResult::Denied("missing file path".into())),
        "file_search" => {
            let path = string_arg(args, "path").unwrap_or(".");
            cfg.can_read_file(Path::new(path))
        }
        "browser" => string_arg(args, "url")
            .and_then(url_host)
            .map(|host| cfg.can_access_network(&host))
            .unwrap_or_else(|| PermissionResult::Denied("missing or invalid URL".into())),
        "web_search" => cfg.can_access_network("duckduckgo.com"),
        "graph_store" => cfg.can_access_db("CREATE"),
        "graph_edit" => cfg.can_access_db("UPDATE"),
        "graph_forget" => cfg.can_access_db("DELETE"),
        "graph_query" | "graph_context" => cfg.can_access_db("SELECT"),
        "provider_mesh" => provider_mesh_permission(cfg, args),
        _ => PermissionResult::Denied(format!("unknown tool: {tool_name}")),
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

fn write_default_config(path: &Path, config: &PermissionsConfig) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "permissions path has no parent",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let contents = toml::to_string_pretty(config).map_err(std::io::Error::other)?;
    let temporary = parent.join(format!(
        ".permissions-{}-{:x}.tmp",
        std::process::id(),
        rand::random::<u64>()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
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
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    parsed.host_str().map(str::to_string)
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

fn requires_shell_confirmation(command: &str) -> bool {
    let trimmed = command.trim_start();
    let first = trimmed.split_whitespace().next().unwrap_or("");
    if matches!(
        first,
        "rm" | "mv"
            | "cp"
            | "ln"
            | "mkdir"
            | "rmdir"
            | "touch"
            | "truncate"
            | "install"
            | "tee"
            | "chmod"
            | "chown"
            | "dd"
            | "mkfs"
            | "mount"
            | "umount"
            | "shutdown"
            | "reboot"
            | "halt"
            | "poweroff"
            | "systemctl"
            | "docker"
            | "ssh"
            | "scp"
            | "rsync"
            | "curl"
            | "wget"
            | "python"
            | "python3"
            | "node"
            | "rustc"
            | "env"
            | "printenv"
    ) {
        return true;
    }

    let second = trimmed.split_whitespace().nth(1).unwrap_or("");
    (first == "git"
        && !matches!(
            second,
            "" | "status"
                | "diff"
                | "log"
                | "show"
                | "rev-parse"
                | "ls-files"
                | "grep"
                | "blame"
                | "shortlog"
                | "describe"
                | "name-rev"
                | "cat-file"
                | "ls-tree"
                | "for-each-ref"
        ))
        || (matches!(first, "cargo" | "npm" | "pnpm")
            && matches!(
                second,
                "add"
                    | "install"
                    | "login"
                    | "owner"
                    | "publish"
                    | "remove"
                    | "uninstall"
                    | "update"
                    | "yank"
            ))
}

fn command_pattern_matches(pattern: &str, command: &str) -> bool {
    let command = command.trim_start();
    command == pattern
        || command
            .strip_prefix(pattern)
            .and_then(|rest| rest.chars().next())
            .is_some_and(char::is_whitespace)
}

fn has_unquoted_shell_control(command: &str) -> bool {
    let mut single_quoted = false;
    let mut double_quoted = false;
    let mut escaped = false;
    let mut chars = command.chars().peekable();
    while let Some(character) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && !single_quoted {
            escaped = true;
            continue;
        }
        if character == '\'' && !double_quoted {
            single_quoted = !single_quoted;
            continue;
        }
        if character == '"' && !single_quoted {
            double_quoted = !double_quoted;
            continue;
        }
        if single_quoted {
            continue;
        }
        if character == '`' {
            return true;
        }
        if character == '$' && chars.peek() == Some(&'(') {
            return true;
        }
        if !double_quoted && matches!(character, ';' | '&' | '|' | '<' | '>' | '\n' | '\r') {
            return true;
        }
    }
    false
}

fn provider_mesh_permission(cfg: &PermissionsConfig, args: &Value) -> PermissionResult {
    let providers = match args.get("providers") {
        None | Some(Value::Null) => vec!["ollama", "deepseek"],
        Some(Value::Array(providers)) if providers.len() <= 3 => {
            let Some(providers) = providers
                .iter()
                .map(Value::as_str)
                .collect::<Option<Vec<_>>>()
            else {
                return PermissionResult::Denied("provider_mesh providers must be strings".into());
            };
            providers
        }
        Some(Value::Array(_)) => {
            return PermissionResult::Denied("provider_mesh accepts at most three providers".into())
        }
        Some(_) => {
            return PermissionResult::Denied("provider_mesh providers must be an array".into())
        }
    };

    if providers.is_empty() {
        return PermissionResult::Denied("provider_mesh requires at least one provider".into());
    }

    let mut confirmation = None;
    for provider in providers {
        let host = match provider {
            "ollama" => "127.0.0.1",
            "deepseek" => "api.deepseek.com",
            "openrouter" => "openrouter.ai",
            _ => {
                return PermissionResult::Denied(format!(
                    "unknown provider_mesh provider: {provider}"
                ))
            }
        };
        match cfg.can_access_network(host) {
            PermissionResult::Denied(reason) => return PermissionResult::Denied(reason),
            PermissionResult::Confirm(reason) => confirmation = Some(reason),
            PermissionResult::Allowed => {}
        }
    }

    confirmation.map_or(PermissionResult::Allowed, PermissionResult::Confirm)
}

fn domain_matches(pattern: &str, host: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let pattern = pattern.trim().trim_end_matches('.').to_ascii_lowercase();
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    host == pattern || host.ends_with(&format!(".{pattern}"))
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

fn resolve_for_policy(path: &Path) -> PathBuf {
    let normalized = normalize_path(path);
    if let Ok(canonical) = std::fs::canonicalize(&normalized) {
        return canonical;
    }

    let mut ancestor = normalized.as_path();
    let mut missing = Vec::new();
    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else {
            break;
        };
        missing.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            break;
        };
        ancestor = parent;
    }
    let mut resolved = std::fs::canonicalize(ancestor).unwrap_or_else(|_| ancestor.to_path_buf());
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    resolved
}

fn path_within_dir(path: &Path, dir: &str) -> bool {
    let base = resolve_for_policy(Path::new(dir));
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
        let pattern_path = Path::new(pattern);
        if pattern_path.is_absolute() {
            return Path::new(s).starts_with(pattern_path);
        }
        return Path::new(s)
            .components()
            .any(|component| component.as_os_str() == pattern);
    }
    if Path::new(pattern).is_absolute() || pattern.contains('/') {
        return wildcard_match(pattern, s);
    }
    Path::new(s).components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|value| wildcard_match(pattern, value))
    })
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut pattern_index, mut value_index) = (0, 0);
    let (mut star_index, mut star_value_index) = (None, 0);

    while value_index < value.len() {
        if pattern_index < pattern.len() && pattern[pattern_index] == value[value_index] {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star_index = Some(pattern_index);
            pattern_index += 1;
            star_value_index = value_index;
        } else if let Some(star) = star_index {
            pattern_index = star + 1;
            star_value_index += 1;
            value_index = star_value_index;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_paths_are_denied_before_broad_home_allow() {
        let cfg = PermissionsConfig::default();
        let result = cfg.can_read_file(Path::new("/home/x1/.ssh/id_ed25519"));
        assert!(matches!(result, PermissionResult::Denied(_)));
        let result = cfg.can_read_file(Path::new("/workspace/.env.production"));
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

    #[test]
    fn default_policy_has_no_developer_specific_paths() {
        let encoded = toml::to_string(&PermissionsConfig::default()).unwrap();
        assert!(!encoded.contains("/Uintellagent"));
        assert!(!encoded.contains("/home/x1"));
        assert!(encoded.contains("config_version = 1"));
    }

    #[test]
    fn shell_allow_list_requires_command_boundaries_and_confirmation() {
        let cfg = PermissionsConfig::default();
        assert!(matches!(
            cfg.can_execute_shell("git status"),
            PermissionResult::Allowed
        ));
        assert!(matches!(
            cfg.can_execute_shell("git-malicious status"),
            PermissionResult::Confirm(_)
        ));
        assert!(matches!(
            cfg.can_execute_shell("git status; curl https://example.com"),
            PermissionResult::Confirm(_)
        ));
        assert!(matches!(
            cfg.can_execute_shell("git push origin main"),
            PermissionResult::Confirm(_)
        ));
        assert!(matches!(
            cfg.can_execute_shell("echo 'a|b'"),
            PermissionResult::Allowed
        ));
        assert!(matches!(
            cfg.can_execute_shell("echo \"a|b\""),
            PermissionResult::Allowed
        ));
        assert!(matches!(
            cfg.can_execute_shell("git fetch origin"),
            PermissionResult::Confirm(_)
        ));
    }

    #[test]
    fn network_policy_matches_only_exact_domains_and_subdomains() {
        let cfg = PermissionsConfig::default();
        assert!(matches!(
            cfg.can_access_network("github.com"),
            PermissionResult::Allowed
        ));
        assert!(matches!(
            cfg.can_access_network("api.github.com"),
            PermissionResult::Allowed
        ));
        assert!(matches!(
            cfg.can_access_network("github.com.evil.example"),
            PermissionResult::Confirm(_)
        ));
        assert!(matches!(
            cfg.can_access_network("git"),
            PermissionResult::Confirm(_)
        ));
    }

    #[test]
    fn future_permission_versions_fail_validation() {
        let mut cfg = PermissionsConfig::default();
        cfg.config_version += 1;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn alpha_permission_defaults_migrate_without_machine_paths() {
        let mut cfg = PermissionsConfig::default();
        cfg.workspace_dirs.push("/Uintellagent".into());
        cfg.allowed_read_paths
            .extend(["/home".into(), "/Uintellagent".into()]);
        cfg.allowed_write_paths
            .extend(["/Uintellagent".into(), "/home/x1".into(), ".".into()]);
        cfg.allowed_commands.extend(["env".into(), "curl".into()]);
        cfg.denied_read_paths.retain(|path| path != ".aws");
        cfg.migrate_alpha_defaults();
        let encoded = toml::to_string(&cfg).unwrap();
        assert!(!encoded.contains("/Uintellagent"));
        assert!(!encoded.contains("/home/x1"));
        assert!(!cfg.allowed_read_paths.contains(&"/home".into()));
        assert!(!cfg.allowed_commands.contains(&"env".into()));
        assert!(!cfg.allowed_commands.contains(&"curl".into()));
        assert!(cfg.denied_read_paths.contains(&".aws".into()));
    }

    #[test]
    fn provider_mesh_and_unknown_tools_fail_closed() {
        let cfg = PermissionsConfig::default();
        assert!(matches!(
            provider_mesh_permission(&cfg, &serde_json::json!({"prompt": "hello"})),
            PermissionResult::Allowed
        ));
        assert!(matches!(
            provider_mesh_permission(
                &cfg,
                &serde_json::json!({"prompt": "hello", "providers": ["unknown"]})
            ),
            PermissionResult::Denied(_)
        ));
        assert!(matches!(
            permission_for_tool_with_config(&cfg, "future_tool", &serde_json::json!({})),
            PermissionResult::Denied(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_workspace_paths_cannot_escape_policy() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "uintell-permissions-{}-{:x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let link = root.join("outside");
        symlink("/etc", &link).unwrap();
        let cfg = PermissionsConfig {
            workspace_dirs: vec![root.to_string_lossy().into_owned()],
            allowed_read_paths: Vec::new(),
            ..PermissionsConfig::default()
        };
        assert!(matches!(
            cfg.can_read_file(&link.join("passwd")),
            PermissionResult::Denied(_)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }
}
