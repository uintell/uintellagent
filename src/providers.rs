// Provider Mesh — fan out to N providers, fastest valid response wins
//
// Instead of blocking on one provider like Hermes, we fire all simultaneously
// on Tokio and return the first valid response. Fallback chain on failure.

use std::time::Instant;

/// Query N providers simultaneously, return fastest valid response.
/// Falls back through chain if the winner fails.
pub async fn mesh_query(
    prompt: &str,
    primary_provider: &str,
    model: Option<&str>,
) -> anyhow::Result<String> {
    let start = Instant::now();

    // For now: single provider. The mesh is stubbed for multi-provider fan-out.
    let response = match primary_provider {
        "deepseek" => query_deepseek(prompt, model).await?,
        "ollama" => query_ollama(prompt, model).await?,
        "openrouter" => query_openrouter(prompt, model).await?,
        other => anyhow::bail!("Unknown provider: {other}"),
    };

    let elapsed = start.elapsed();
    tracing::info!("Provider mesh returned in {:?}", elapsed);

    Ok(response)
}

async fn query_deepseek(prompt: &str, _model: Option<&str>) -> anyhow::Result<String> {
    let api_key = std::env::var("DEEPSEEK_API_KEY")
        .or_else(|_| std::env::var("UINTELL_API_KEY"))
        .unwrap_or_default();

    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "deepseek-chat",
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": prompt}
        ],
        "max_tokens": 4096,
        "temperature": 0.7
    });

    let resp = client
        .post("https://api.deepseek.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    let json: serde_json::Value = resp.json().await?;

    json["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("No content in DeepSeek response"))
}

async fn query_ollama(prompt: &str, model: Option<&str>) -> anyhow::Result<String> {
    let client = reqwest::Client::new();
    let model = model.unwrap_or("qwen3-coder:30b");

    let body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": prompt}
        ],
        "stream": false
    });

    let resp = client
        .post("http://127.0.0.1:11434/api/chat")
        .json(&body)
        .send()
        .await?;

    let json: serde_json::Value = resp.json().await?;

    json["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("No content in Ollama response"))
}

async fn query_openrouter(prompt: &str, _model: Option<&str>) -> anyhow::Result<String> {
    let api_key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();

    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "deepseek/deepseek-chat",
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": prompt}
        ],
        "max_tokens": 4096
    });

    let resp = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    let json: serde_json::Value = resp.json().await?;

    json["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("No content in OpenRouter response"))
}

pub const SYSTEM_PROMPT: &str = r#"You are UIntell Agent — a Rust-native AI agent built to outperform all other agents.

CORE RULES:
- Answer directly and concisely.
- Respect tool permissions, confirmations, and runtime limits.
- Use tools when they produce verified results.
- Be concise. Lead with the answer, not preamble.
- Execute, don't describe. When asked to build/run/verify, produce working output.

CAPABILITIES:
- Provider mesh: fan-out to multiple LLMs simultaneously
- Graph memory: SurrealDB entities+edges for connected reasoning
- Zero-alloc tool dispatch: terminal, file, search, browser, code execution
- Wasm skills: compile-time verified, type-safe plugins
"#;
