// Shared tool result type — all tools return structured JSON using this format.
//
// Every tool call produces: success, tool_name, data/error, elapsed_ms, audit entry.

#![allow(dead_code)]

use serde::Serialize;

#[derive(Serialize, Debug, Clone)]
pub struct ToolOutput {
    pub success: bool,
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

impl ToolOutput {
    pub fn ok(tool: &str, data: serde_json::Value, elapsed_ms: u64) -> String {
        let out = Self {
            success: true,
            tool: tool.into(),
            data: Some(data),
            error: None,
            elapsed_ms,
        };
        serde_json::to_string(&out).unwrap_or_default()
    }

    pub fn err(tool: &str, error: &str, elapsed_ms: u64) -> String {
        let out = Self {
            success: false,
            tool: tool.into(),
            data: None,
            error: Some(error.into()),
            elapsed_ms,
        };
        serde_json::to_string(&out).unwrap_or_default()
    }
}

/// Audit logger — appends JSON lines to ~/.uintell/audit.log
pub fn audit(action: &str, tool: &str, detail: &str, approved: bool) {
    let entry = serde_json::json!({
        "timestamp": chrono_now(),
        "action": action,
        "tool": tool,
        "detail": detail,
        "approved": approved,
    });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(audit_path())
    {
        use std::io::Write;
        let _ = writeln!(
            file,
            "{}",
            serde_json::to_string(&entry).unwrap_or_default()
        );
    }
}

fn audit_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home)
        .join(".uintell")
        .join("audit.log")
}

fn chrono_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, mo, d, h, mi, s) = unix_to_ymdhms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn unix_to_ymdhms(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let mut days = (secs / 86400) as i64;
    let time = secs % 86400;
    let mut y = 1970i64;
    loop {
        let diy = if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
            366
        } else {
            365
        };
        if days < diy {
            break;
        }
        days -= diy;
        y += 1;
    }
    let mdays = if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut mo = 0;
    while mo < 12 && days >= mdays[mo] as i64 {
        days -= mdays[mo] as i64;
        mo += 1;
    }
    (
        y,
        mo as u32 + 1,
        days as u32 + 1,
        (time / 3600) as u32,
        ((time % 3600) / 60) as u32,
        (time % 60) as u32,
    )
}
