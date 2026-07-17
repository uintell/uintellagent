// Provider Mesh — fan out to multiple LLMs, fastest valid response wins
//
// Tool that the agent can call to query multiple providers simultaneously.
// Returns the first valid response with provider attribution.

use rig_core::completion::ToolDefinition;
use rig_core::tool::Tool;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Instant;
use tokio::task::JoinSet;

#[derive(Deserialize)]
pub struct MeshArgs {
    prompt: String,
    #[serde(default)]
    providers: Option<Vec<String>>,
}

#[derive(Debug, thiserror::Error)]
#[error("mesh error")]
pub struct MeshError;

pub struct ProviderMesh;

impl Tool for ProviderMesh {
    const NAME: &'static str = "provider_mesh";

    type Error = MeshError;
    type Args = MeshArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "provider_mesh".to_string(),
            description: "Query multiple LLM providers simultaneously. Returns fastest valid response. Providers: deepseek, ollama, openrouter. Use for getting diverse perspectives or when latency matters.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "The prompt to send to all providers" },
                    "providers": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Providers to query (default: all available)"
                    }
                },
                "required": ["prompt"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let providers = args
            .providers
            .unwrap_or_else(|| vec!["ollama".into(), "deepseek".into()]);

        let start = Instant::now();
        let mut set = JoinSet::new();

        for provider in &providers {
            let provider = provider.clone();
            let prompt = args.prompt.clone();
            set.spawn(async move {
                match provider.as_str() {
                    "ollama" => query_ollama_mesh(&prompt).await,
                    "deepseek" => query_deepseek_mesh(&prompt).await,
                    "openrouter" => query_openrouter_mesh(&prompt).await,
                    _ => Err(format!("Unknown provider: {provider}")),
                }
            });
        }

        // Take first successful response
        while let Some(result) = set.join_next().await {
            match result {
                Ok(Ok((provider, response))) => {
                    let elapsed = start.elapsed();
                    let remaining = set.len();
                    // Abort remaining tasks
                    set.abort_all();
                    if remaining > 0 {
                        return Ok(format!(
                            "[{provider}] ({:.1?}, {remaining} others cancelled)\n\n{response}",
                            elapsed
                        ));
                    }
                    return Ok(format!("[{provider}] ({:.1?})\n\n{response}", elapsed));
                }
                Ok(Err(_e)) => {
                    // Provider failed, try next
                    continue;
                }
                Err(_) => continue,
            }
        }

        Ok("All providers failed.".into())
    }
}

// ── Provider query functions ───────────────────────────────────

async fn query_deepseek_mesh(prompt: &str) -> Result<(String, String), String> {
    let api_key =
        std::env::var("DEEPSEEK_API_KEY").map_err(|_| "No DEEPSEEK_API_KEY".to_string())?;

    let client = reqwest::Client::new();
    let body = json!({
        "model": "deepseek-chat",
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 2048,
        "temperature": 0.7
    });

    let resp = client
        .post("https://api.deepseek.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("DeepSeek: {e}"))?;

    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("DeepSeek parse: {e}"))?;
    let text = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or("DeepSeek: no content")?
        .to_string();

    Ok(("deepseek".into(), text))
}

async fn query_ollama_mesh(prompt: &str) -> Result<(String, String), String> {
    let client = reqwest::Client::new();
    let body = json!({
        "model": "hf.co/unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF:UD-Q4_K_XL",
        "messages": [{"role": "user", "content": prompt}],
        "stream": false
    });

    let resp = client
        .post("http://127.0.0.1:11434/api/chat")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Ollama: {e}"))?;

    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("Ollama parse: {e}"))?;
    let text = json["message"]["content"]
        .as_str()
        .ok_or("Ollama: no content")?
        .to_string();

    Ok(("ollama".into(), text))
}

async fn query_openrouter_mesh(prompt: &str) -> Result<(String, String), String> {
    let api_key =
        std::env::var("OPENROUTER_API_KEY").map_err(|_| "No OPENROUTER_API_KEY".to_string())?;

    let client = reqwest::Client::new();
    let body = json!({
        "model": "deepseek/deepseek-chat",
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 2048
    });

    let resp = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("OpenRouter: {e}"))?;

    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("OpenRouter parse: {e}"))?;
    let text = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or("OpenRouter: no content")?
        .to_string();

    Ok(("openrouter".into(), text))
}
