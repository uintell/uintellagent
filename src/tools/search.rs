// Web Search tool — DuckDuckGo HTML search (no API key needed)
use rig_core::completion::ToolDefinition;
use rig_core::tool::Tool;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
pub struct WebSearchArgs {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct WebSearchError {
    message: String,
}

impl WebSearchError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub struct WebSearch;

impl Tool for WebSearch {
    const NAME: &'static str = "web_search";

    type Error = WebSearchError;
    type Args = WebSearchArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "web_search".to_string(),
            description: "Search the web using DuckDuckGo. Returns titles, URLs, and snippets. No API key required.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query" },
                    "limit": { "type": "integer", "description": "Max results (default 10, max 20)" }
                },
                "required": ["query"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let permission_args = json!({ "query": &args.query }).to_string();
        if let Err(reason) = crate::permissions::enforce_tool_call(Self::NAME, &permission_args) {
            return Ok(reason);
        }

        let limit = args.limit.unwrap_or(10).min(20);

        let client = reqwest::Client::builder()
            .user_agent("UIntellAgent/0.3 (search)")
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|error| WebSearchError::new(format!("build search client: {error}")))?;

        let url = format!(
            "https://html.duckduckgo.com/html/?q={}",
            urlencoding::encode(&args.query)
        );

        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|error| WebSearchError::new(format!("search DuckDuckGo: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(WebSearchError::new(format!(
                "DuckDuckGo search returned HTTP {status}"
            )));
        }
        let body = response
            .text()
            .await
            .map_err(|error| WebSearchError::new(format!("read search response: {error}")))?;

        let results = parse_ddg_html(&body, limit);

        if results.is_empty() {
            return Ok(format!("No results for: {}", args.query));
        }

        let mut out = format!("Results for: {}\n\n", args.query);
        for (i, (title, snippet, url)) in results.iter().enumerate() {
            out.push_str(&format!(
                "{}. {}\n   {}\n   {}\n\n",
                i + 1,
                title,
                url,
                snippet
            ));
        }
        Ok(out)
    }
}

fn parse_ddg_html(html: &str, limit: usize) -> Vec<(String, String, String)> {
    let mut results = Vec::new();

    // Extract result blocks using simple string scanning
    let mut remaining = html;
    while results.len() < limit {
        // Find next result title
        let title_start = match remaining.find("class=\"result__a\"") {
            Some(pos) => pos + 16,
            None => break,
        };

        let after_title = &remaining[title_start..];
        let title_end = match after_title.find("</a>") {
            Some(pos) => title_start + pos,
            None => break,
        };
        let title_raw = &remaining[title_start..title_end];

        // Extract href
        let title = strip_tags(title_raw);
        let url = extract_href(title_raw).unwrap_or_else(|| "?".into());

        // Find snippet
        let snippet_start = match remaining[title_end..].find("class=\"result__snippet\"") {
            Some(pos) => title_end + pos + 22,
            None => {
                remaining = &remaining[title_end + 4..];
                continue;
            }
        };
        let after_snippet = &remaining[snippet_start..];
        let snippet_end = match after_snippet.find("</") {
            Some(pos) => snippet_start + pos,
            None => break,
        };
        let snippet = strip_tags(&remaining[snippet_start..snippet_end]);

        results.push((title, snippet, url));
        remaining = &remaining[snippet_end..];
    }

    results
}

fn strip_tags(s: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(c),
            _ => {}
        }
    }
    result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .trim()
        .to_string()
}

fn extract_href(s: &str) -> Option<String> {
    let start = s.find("href=\"")? + 6;
    let end = s[start..].find('"')?;
    let mut url = s[start..start + end].to_string();
    // Resolve relative URLs
    if url.starts_with("//") {
        url = format!("https:{url}");
    } else if url.starts_with('/') {
        url = format!("https://duckduckgo.com{url}");
    }
    Some(url)
}
