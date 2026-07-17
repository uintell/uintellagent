// Provider Mesh — fan out to multiple LLMs, fastest valid response wins
//
// Tool that the agent can call to query multiple providers simultaneously.
// Returns the first valid response with provider attribution.

use rig_core::completion::ToolDefinition;
use rig_core::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Instant;
use tokio::task::JoinSet;

const MAX_MESH_PROMPT_CHARS: usize = 32_000;
const MAX_PROVIDER_RESPONSE_BYTES: usize = 1_000_000;

#[derive(Deserialize, Serialize)]
pub struct MeshArgs {
    prompt: String,
    #[serde(default)]
    providers: Option<Vec<String>>,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct MeshError {
    message: String,
}

impl MeshError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

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
        let permission_args = serde_json::to_string(&args)
            .map_err(|error| MeshError::new(format!("encode mesh request: {error}")))?;
        if let Err(reason) = crate::permissions::enforce_tool_call(Self::NAME, &permission_args) {
            return Ok(reason);
        }
        if args.prompt.chars().count() > MAX_MESH_PROMPT_CHARS {
            return Err(MeshError::new(format!(
                "mesh prompt exceeds {MAX_MESH_PROMPT_CHARS} characters"
            )));
        }

        let providers = args
            .providers
            .unwrap_or_else(|| vec!["ollama".into(), "deepseek".into()]);
        if providers.is_empty() || providers.len() > 3 {
            return Err(MeshError::new(
                "provider mesh requires between one and three providers",
            ));
        }

        let start = Instant::now();
        let mut set = JoinSet::new();
        let mut seen = HashSet::new();

        for provider in &providers {
            if !matches!(provider.as_str(), "ollama" | "deepseek" | "openrouter") {
                return Err(MeshError::new(format!("unknown provider: {provider}")));
            }
            if !seen.insert(provider.as_str()) {
                continue;
            }
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
        let mut failures = Vec::new();
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
                Ok(Err(error)) => failures.push(error),
                Err(error) => failures.push(format!("provider task failed: {error}")),
            }
        }

        Err(MeshError::new(format!(
            "all providers failed: {}",
            failures.join("; ")
        )))
    }
}

// ── Provider query functions ───────────────────────────────────

async fn query_deepseek_mesh(prompt: &str) -> Result<(String, String), String> {
    let api_key =
        std::env::var("DEEPSEEK_API_KEY").map_err(|_| "No DEEPSEEK_API_KEY".to_string())?;

    let client = mesh_client()?;
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

    let json = parse_provider_response(resp, "DeepSeek").await?;
    let text = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or("DeepSeek: no content")?
        .to_string();

    Ok(("deepseek".into(), text))
}

async fn query_ollama_mesh(prompt: &str) -> Result<(String, String), String> {
    let client = mesh_client()?;
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

    let json = parse_provider_response(resp, "Ollama").await?;
    let text = json["message"]["content"]
        .as_str()
        .ok_or("Ollama: no content")?
        .to_string();

    Ok(("ollama".into(), text))
}

async fn query_openrouter_mesh(prompt: &str) -> Result<(String, String), String> {
    let api_key =
        std::env::var("OPENROUTER_API_KEY").map_err(|_| "No OPENROUTER_API_KEY".to_string())?;

    let client = mesh_client()?;
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

    let json = parse_provider_response(resp, "OpenRouter").await?;
    let text = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or("OpenRouter: no content")?
        .to_string();

    Ok(("openrouter".into(), text))
}

fn mesh_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("build provider client: {error}"))
}

async fn parse_provider_response(
    response: reqwest::Response,
    provider: &str,
) -> Result<Value, String> {
    let response = crate::http_body::read_response(response, MAX_PROVIDER_RESPONSE_BYTES).await?;
    if response.truncated {
        return Err(format!(
            "{provider} response exceeded {MAX_PROVIDER_RESPONSE_BYTES} bytes"
        ));
    }
    if !response.status.is_success() {
        let detail = String::from_utf8_lossy(&response.bytes)
            .chars()
            .take(500)
            .collect::<String>();
        return Err(format!(
            "{provider} returned HTTP {}: {detail}",
            response.status
        ));
    }
    serde_json::from_slice(&response.bytes).map_err(|error| format!("{provider} parse: {error}"))
}
