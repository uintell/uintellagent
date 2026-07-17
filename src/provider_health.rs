use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderHealth {
    Ready(String),
    Unavailable(String),
}

impl ProviderHealth {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    pub fn detail(&self) -> &str {
        match self {
            Self::Ready(detail) | Self::Unavailable(detail) => detail,
        }
    }

    pub fn badge(&self) -> &'static str {
        if self.is_ready() {
            "provider:ready"
        } else {
            "provider:offline"
        }
    }

    pub fn require_ready(&self) -> anyhow::Result<()> {
        match self {
            Self::Ready(_) => Ok(()),
            Self::Unavailable(detail) => anyhow::bail!(detail.clone()),
        }
    }
}

pub async fn check_deepseek() -> ProviderHealth {
    let key = std::env::var("DEEPSEEK_API_KEY").unwrap_or_default();
    if key.trim().is_empty() {
        return ProviderHealth::Unavailable(
            "DeepSeek is not configured. Set DEEPSEEK_API_KEY or start with --ollama.".into(),
        );
    }

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return ProviderHealth::Unavailable(format!(
                "DeepSeek preflight client failed: {error}"
            ));
        }
    };
    match client
        .get("https://api.deepseek.com/models")
        .bearer_auth(key)
        .send()
        .await
    {
        Ok(response) => classify_deepseek_status(response.status().as_u16()),
        Err(error) if error.is_timeout() => ProviderHealth::Unavailable(
            "DeepSeek preflight timed out after 8 seconds; check network access.".into(),
        ),
        Err(error) => ProviderHealth::Unavailable(format!("DeepSeek is unreachable: {error}")),
    }
}

pub async fn check_ollama(model: &str) -> ProviderHealth {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return ProviderHealth::Unavailable(format!("Ollama preflight client failed: {error}"));
        }
    };
    match client.get("http://127.0.0.1:11434/api/tags").send().await {
        Ok(response) if response.status().is_success() => {
            match response.json::<serde_json::Value>().await {
                Ok(payload) if ollama_has_model(&payload, model) => {
                    ProviderHealth::Ready(format!("Ollama is ready · {model}"))
                }
                Ok(_) => ProviderHealth::Unavailable(format!(
                    "Ollama is running but model `{model}` is not installed. Run `ollama pull {model}`."
                )),
                Err(error) => ProviderHealth::Unavailable(format!(
                    "Ollama is running but its model list is invalid: {error}"
                )),
            }
        }
        Ok(response) => ProviderHealth::Unavailable(format!(
            "Ollama returned HTTP {}; start it with `ollama serve`.",
            response.status().as_u16()
        )),
        Err(error) if error.is_timeout() => ProviderHealth::Unavailable(
            "Ollama preflight timed out; start it with `ollama serve`.".into(),
        ),
        Err(error) => ProviderHealth::Unavailable(format!(
            "Ollama is unavailable at 127.0.0.1:11434: {error}"
        )),
    }
}

fn ollama_has_model(payload: &serde_json::Value, requested: &str) -> bool {
    let requested = requested.strip_suffix(":latest").unwrap_or(requested);
    payload
        .get("models")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            model
                .get("name")
                .or_else(|| model.get("model"))
                .and_then(serde_json::Value::as_str)
        })
        .any(|installed| installed.strip_suffix(":latest").unwrap_or(installed) == requested)
}

fn classify_deepseek_status(status: u16) -> ProviderHealth {
    match status {
        200..=299 => ProviderHealth::Ready("DeepSeek authentication verified".into()),
        401 => ProviderHealth::Unavailable(
            "DeepSeek rejected DEEPSEEK_API_KEY (HTTP 401). Replace the key and restart.".into(),
        ),
        403 => ProviderHealth::Unavailable(
            "DeepSeek denied this account or key (HTTP 403). Check account access.".into(),
        ),
        429 => ProviderHealth::Unavailable(
            "DeepSeek is rate-limited (HTTP 429). Wait or check account quota.".into(),
        ),
        status => {
            ProviderHealth::Unavailable(format!("DeepSeek preflight failed with HTTP {status}."))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_preflight_classifies_authentication_failures() {
        assert!(classify_deepseek_status(200).is_ready());
        assert!(classify_deepseek_status(401).detail().contains("rejected"));
        assert!(classify_deepseek_status(429)
            .detail()
            .contains("rate-limited"));
        assert!(!classify_deepseek_status(503).is_ready());
    }

    #[test]
    fn ollama_preflight_requires_the_selected_model() {
        let payload = serde_json::json!({
            "models": [{"name": "qwen:latest"}, {"model": "deepseek-r1:7b"}]
        });
        assert!(ollama_has_model(&payload, "qwen"));
        assert!(ollama_has_model(&payload, "deepseek-r1:7b"));
        assert!(!ollama_has_model(&payload, "missing"));
    }
}
