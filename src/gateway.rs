// Gateway — hardened axum HTTP API
//
// Endpoints:
//   GET  /health           — health check (no auth)
//   GET  /ready            — readiness check (DB + provider status)
//   POST /chat             — send message, get JSON response (API key required)
//
// Security:
//   - API key auth via X-API-Key header or Authorization: Bearer
//   - Rate limiting: 60 req/min per key
//   - Request IDs: X-Request-Id header or auto-generated UUID
//   - Structured errors with error codes
//   - Timeout: 120s per request
//   - CORS: configurable allowed origins

use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use rig_core::agent::Agent;
use rig_core::completion::{CompletionModel, Message, Prompt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

// ═══════════════════════════════════════════════════════════════
// CONFIG
// ═══════════════════════════════════════════════════════════════

const MAX_REQUESTS_PER_MINUTE: u32 = 60;
const REQUEST_TIMEOUT_SECS: u64 = 120;
const MAX_MESSAGE_CHARS: usize = 32_000;
const MAX_SESSION_ID_CHARS: usize = 128;

fn api_key() -> Option<String> {
    std::env::var("UINTELL_API_KEY")
        .or_else(|_| std::env::var("DEEPSEEK_API_KEY"))
        .ok()
        .filter(|key| !key.trim().is_empty())
}

// ═══════════════════════════════════════════════════════════════
// REQUEST / RESPONSE TYPES
// ═══════════════════════════════════════════════════════════════

#[derive(Deserialize)]
struct ChatRequest {
    message: String,
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Serialize)]
struct ChatResponse {
    id: String,
    status: String,
    response: Option<String>,
    provider: String,
    usage: Option<UsageInfo>,
    error: Option<ErrorInfo>,
}

#[derive(Serialize)]
struct UsageInfo {
    prompt_tokens: u64,
    completion_tokens: u64,
}

#[derive(Serialize)]
struct ErrorInfo {
    code: String,
    message: String,
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    version: String,
    uptime_secs: u64,
}

// ═══════════════════════════════════════════════════════════════
// APP STATE
// ═══════════════════════════════════════════════════════════════

struct GatewayState<M: CompletionModel> {
    agent: Mutex<Agent<M>>,
    provider: String,
    start_time: Instant,
    /// Rate limiter: API key → (window_start, count)
    rate_limits: Mutex<HashMap<String, (Instant, u32)>>,
    /// In-memory conversation history keyed by validated session ID.
    histories: Mutex<HashMap<String, Vec<Message>>>,
}

// ═══════════════════════════════════════════════════════════════
// MIDDLEWARE
// ═══════════════════════════════════════════════════════════════

/// Check API key and rate limit
async fn auth_middleware<M: CompletionModel>(
    State(state): State<Arc<GatewayState<M>>>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    if request.uri().path() == "/health" {
        return next.run(request).await;
    }

    let key = extract_api_key(&headers);
    let Some(expected) = api_key() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "AUTH_NOT_CONFIGURED",
            "Set UINTELL_API_KEY before enabling gateway endpoints.",
        );
    };

    if key != expected {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "Invalid or missing API key. Use X-API-Key or Authorization: Bearer header.",
        );
    }

    // Rate limiting
    let mut limits = state.rate_limits.lock().await;
    let entry = limits.entry(key).or_insert((Instant::now(), 0));
    if entry.0.elapsed() > Duration::from_secs(60) {
        *entry = (Instant::now(), 0);
    }
    entry.1 += 1;
    if entry.1 > MAX_REQUESTS_PER_MINUTE {
        return error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "RATE_LIMITED",
            &format!("Max {MAX_REQUESTS_PER_MINUTE} requests per minute. Retry after 60s."),
        );
    }

    next.run(request).await
}

fn extract_api_key(headers: &HeaderMap) -> String {
    // X-API-Key header
    if let Some(val) = headers.get("x-api-key") {
        if let Ok(s) = val.to_str() {
            return s.to_string();
        }
    }
    // Authorization: Bearer <key>
    if let Some(val) = headers.get("authorization") {
        if let Ok(s) = val.to_str() {
            if let Some(key) = s.strip_prefix("Bearer ") {
                return key.to_string();
            }
        }
    }
    String::new()
}

fn error_response(status: StatusCode, code: &str, message: &str) -> axum::response::Response {
    let body = serde_json::json!({
        "error": {
            "code": code,
            "message": message,
        }
    });
    let mut resp = axum::response::Json(body).into_response();
    *resp.status_mut() = status;
    resp
}

// ═══════════════════════════════════════════════════════════════
// CORS
// ═══════════════════════════════════════════════════════════════

fn cors_layer() -> tower_http::cors::CorsLayer {
    let layer = tower_http::cors::CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            HeaderName::from_static("x-api-key"),
            HeaderName::from_static("x-request-id"),
        ]);

    if std::env::var("UINTELL_CORS_ALLOW_ANY").as_deref() == Ok("1") {
        return layer.allow_origin(tower_http::cors::Any);
    }

    let origins: Vec<HeaderValue> = std::env::var("UINTELL_CORS_ORIGINS")
        .unwrap_or_else(|_| "http://localhost:3000,http://127.0.0.1:3000".into())
        .split(',')
        .filter_map(|origin| origin.trim().parse().ok())
        .collect();
    layer.allow_origin(origins)
}

// ═══════════════════════════════════════════════════════════════
// HANDLERS
// ═══════════════════════════════════════════════════════════════

async fn health<M: CompletionModel>(
    State(state): State<Arc<GatewayState<M>>>,
) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        uptime_secs: state.start_time.elapsed().as_secs(),
    })
}

async fn ready<M: CompletionModel + 'static>(
    State(state): State<Arc<GatewayState<M>>>,
) -> Json<serde_json::Value> {
    // Check agent is alive with a quick test
    let agent = state.agent.lock().await;
    let ok = agent.prompt("ping").await.is_ok();
    Json(serde_json::json!({
        "status": if ok { "ready" } else { "degraded" },
        "provider": state.provider,
    }))
}

async fn chat<M: CompletionModel + 'static>(
    State(state): State<Arc<GatewayState<M>>>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> impl IntoResponse {
    let request_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let request_id = if request_id.is_empty() {
        uuid_v4()
    } else {
        request_id
    };

    if let Err(error) = validate_chat_request(&req) {
        return Json(ChatResponse {
            id: request_id,
            status: "error".into(),
            response: None,
            provider: state.provider.clone(),
            usage: None,
            error: Some(error),
        });
    }

    let agent = state.agent.lock().await;
    let history = match req.session_id.as_deref() {
        Some(session_id) => state
            .histories
            .lock()
            .await
            .get(session_id)
            .cloned()
            .unwrap_or_default(),
        None => Vec::new(),
    };

    match tokio::time::timeout(
        Duration::from_secs(REQUEST_TIMEOUT_SECS),
        agent.prompt(&req.message).history(history).max_turns(12),
    )
    .await
    {
        Ok(Ok(response)) => {
            if let Some(session_id) = req.session_id {
                let mut histories = state.histories.lock().await;
                let messages = histories.entry(session_id).or_default();
                messages.push(Message::user(req.message));
                messages.push(Message::assistant(response.clone()));
                if messages.len() > 40 {
                    messages.drain(..messages.len() - 40);
                }
            }
            Json(ChatResponse {
                id: request_id,
                status: "ok".into(),
                response: Some(response),
                provider: state.provider.clone(),
                usage: None,
                error: None,
            })
        }
        Ok(Err(e)) => Json(ChatResponse {
            id: request_id,
            status: "error".into(),
            response: None,
            provider: state.provider.clone(),
            usage: None,
            error: Some(ErrorInfo {
                code: "AGENT_ERROR".into(),
                message: format!("{e}"),
            }),
        }),
        Err(_) => Json(ChatResponse {
            id: request_id,
            status: "timeout".into(),
            response: None,
            provider: state.provider.clone(),
            usage: None,
            error: Some(ErrorInfo {
                code: "TIMEOUT".into(),
                message: format!("Request exceeded {REQUEST_TIMEOUT_SECS}s timeout"),
            }),
        }),
    }
}

fn validate_chat_request(req: &ChatRequest) -> Result<(), ErrorInfo> {
    if req.message.trim().is_empty() {
        return Err(ErrorInfo {
            code: "EMPTY_MESSAGE".into(),
            message: "message must not be empty".into(),
        });
    }
    if req.message.chars().count() > MAX_MESSAGE_CHARS {
        return Err(ErrorInfo {
            code: "MESSAGE_TOO_LARGE".into(),
            message: format!("message must be at most {MAX_MESSAGE_CHARS} characters"),
        });
    }
    if let Some(session_id) = &req.session_id {
        if !valid_session_id(session_id) {
            return Err(ErrorInfo {
                code: "INVALID_SESSION_ID".into(),
                message: "session_id may contain only ASCII letters, digits, '.', '_' and '-'"
                    .into(),
            });
        }
    }
    Ok(())
}

fn valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= MAX_SESSION_ID_CHARS
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn uuid_v4() -> String {
    use std::fmt::Write;
    let mut r: [u8; 16] = rand::random();
    r[6] = (r[6] & 0x0f) | 0x40;
    r[8] = (r[8] & 0x3f) | 0x80;
    let mut s = String::with_capacity(36);
    for (i, b) in r.iter().enumerate() {
        if i == 4 || i == 6 || i == 8 || i == 10 {
            s.push('-');
        }
        write!(s, "{b:02x}").ok();
    }
    s
}

// ═══════════════════════════════════════════════════════════════
// ENTRY POINT
// ═══════════════════════════════════════════════════════════════

pub async fn serve<M>(agent: Agent<M>, provider: &str, addr: &str) -> anyhow::Result<()>
where
    M: CompletionModel + Send + 'static,
{
    let state = Arc::new(GatewayState {
        agent: Mutex::new(agent),
        provider: provider.to_string(),
        start_time: Instant::now(),
        rate_limits: Mutex::new(HashMap::new()),
        histories: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/health", get(health::<M>))
        .route("/ready", get(ready::<M>))
        .route("/chat", post(chat::<M>))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware::<M>,
        ))
        .layer(cors_layer())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("Gateway: http://{addr}");
    eprintln!("Health:  http://{addr}/health");
    eprintln!("Chat:    POST http://{addr}/chat");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_chat_request_size_and_session_id() {
        let valid = ChatRequest {
            message: "hello".into(),
            session_id: Some("session_1.2-3".into()),
        };
        assert!(validate_chat_request(&valid).is_ok());

        let empty = ChatRequest {
            message: "   ".into(),
            session_id: None,
        };
        assert_eq!(
            validate_chat_request(&empty).unwrap_err().code,
            "EMPTY_MESSAGE"
        );

        let bad_session = ChatRequest {
            message: "hello".into(),
            session_id: Some("../bad".into()),
        };
        assert_eq!(
            validate_chat_request(&bad_session).unwrap_err().code,
            "INVALID_SESSION_ID"
        );
    }

    #[test]
    fn uuid_v4_has_expected_shape() {
        let id = uuid_v4();
        assert_eq!(id.len(), 36);
        assert_eq!(id.chars().nth(14), Some('4'));
        assert!(matches!(id.chars().nth(19), Some('8' | '9' | 'a' | 'b')));
    }
}
