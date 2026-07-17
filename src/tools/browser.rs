// Browser tool — fetch web pages via reqwest with permission checks
use rig_core::completion::ToolDefinition;
use rig_core::tool::Tool;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
pub struct BrowserArgs {
    url: String,
    #[serde(default)]
    text_only: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct BrowserError {
    message: String,
}

impl BrowserError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub struct Browser;

impl Tool for Browser {
    const NAME: &'static str = "browser";

    type Error = BrowserError;
    type Args = BrowserArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "browser".to_string(),
            description:
                "Fetch a web page. Returns HTML by default, or stripped text if text_only=true."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL to fetch" },
                    "text_only": { "type": "boolean", "description": "Return plain text instead of HTML" }
                },
                "required": ["url"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let permission_args = json!({ "url": &args.url }).to_string();
        if let Err(reason) = crate::permissions::enforce_tool_call(Self::NAME, &permission_args) {
            return Ok(reason);
        }

        let client = reqwest::Client::builder()
            .user_agent("UIntellAgent/0.3")
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|error| BrowserError::new(format!("build HTTP client: {error}")))?;

        let resp = client
            .get(&args.url)
            .send()
            .await
            .map_err(|error| BrowserError::new(format!("fetch {}: {error}", args.url)))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|error| BrowserError::new(format!("read {} response: {error}", args.url)))?;

        if args.text_only {
            let text = strip_html(&body);
            Ok(format!("[HTTP {status}]\n{text}"))
        } else {
            let original_bytes = body.len();
            let (body, truncated) = truncate_chars(&body, 100_000);
            Ok(if truncated {
                format!(
                    "[HTTP {status}] {body}... (truncated from {} bytes)",
                    original_bytes
                )
            } else {
                format!("[HTTP {status}]\n{body}")
            })
        }
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> (String, bool) {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect();
    (prefix, chars.next().is_some())
}

fn strip_html(html: &str) -> String {
    let mut in_tag = false;
    let mut result = String::new();
    for c in html.chars() {
        match c {
            '<' => {
                in_tag = true;
            }
            '>' => {
                in_tag = false;
            }
            _ if !in_tag => result.push(c),
            _ => {}
        }
    }
    result = result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    let mut clean = String::new();
    let mut last_nl = false;
    for line in result.lines() {
        let t = line.trim();
        if t.is_empty() {
            if !last_nl {
                clean.push('\n');
                last_nl = true;
            }
        } else {
            clean.push_str(t);
            clean.push('\n');
            last_nl = false;
        }
    }
    let (clean, truncated) = truncate_chars(&clean, 50_000);
    if truncated {
        format!("{clean}...")
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::{strip_html, truncate_chars};

    #[test]
    fn unicode_content_truncates_on_character_boundaries() {
        assert_eq!(truncate_chars("αβγ", 2), ("αβ".into(), true));
        let html = format!("<p>{}</p>", "界".repeat(50_100));
        let text = strip_html(&html);
        assert!(text.ends_with("..."));
        assert_eq!(text.chars().count(), 50_003);
    }
}
