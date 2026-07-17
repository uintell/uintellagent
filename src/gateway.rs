// Gateway — hardened axum HTTP API
//
// Endpoints:
//   GET  /health           — health check (no auth)
//   GET  /ready            — authenticated process readiness
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
    extract::DefaultBodyLimit,
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
const MAX_REQUEST_ID_CHARS: usize = 128;
const MAX_SESSIONS: usize = 64;
const MAX_SESSION_MESSAGES: usize = 20;
const MAX_REQUEST_BYTES: usize = 128 * 1024;

fn api_key() -> Option<String> {
    std::env::var("UINTELL_API_KEY")
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
    histories: Mutex<HashMap<String, SessionHistory>>,
}

struct SessionHistory {
    messages: Vec<Message>,
    last_used: Instant,
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

    if !constant_time_eq(key.as_bytes(), expected.as_bytes()) {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "Invalid or missing API key. Use X-API-Key or Authorization: Bearer header.",
        );
    }

    // Rate limiting
    let key_id = blake3::hash(key.as_bytes()).to_hex().to_string();
    let mut limits = state.rate_limits.lock().await;
    let entry = limits.entry(key_id).or_insert((Instant::now(), 0));
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

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let max_len = left.len().max(right.len());
    let mut difference = left.len() ^ right.len();
    for index in 0..max_len {
        let left_byte = left.get(index).copied().unwrap_or_default();
        let right_byte = right.get(index).copied().unwrap_or_default();
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
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
    Json(serde_json::json!({
        "status": "ready",
        "provider": state.provider,
    }))
}

async fn chat<M: CompletionModel + 'static>(
    State(state): State<Arc<GatewayState<M>>>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> axum::response::Response {
    let request_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|value| valid_request_id(value))
        .map(str::to_string)
        .unwrap_or_else(uuid_v4);

    if let Err(error) = validate_chat_request(&req) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ChatResponse {
                id: request_id,
                status: "error".into(),
                response: None,
                provider: state.provider.clone(),
                usage: None,
                error: Some(error),
            }),
        )
            .into_response();
    }

    let history = match req.session_id.as_deref() {
        Some(session_id) => {
            let mut histories = state.histories.lock().await;
            histories
                .get_mut(session_id)
                .map_or_else(Vec::new, |history| {
                    history.last_used = Instant::now();
                    history.messages.clone()
                })
        }
        None => Vec::new(),
    };

    match tokio::time::timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS), async {
        let agent = state.agent.lock().await;
        agent
            .prompt(&req.message)
            .history(history)
            .max_turns(12)
            .await
    })
    .await
    {
        Ok(Ok(response)) => {
            if let Some(session_id) = req.session_id {
                let mut histories = state.histories.lock().await;
                if !histories.contains_key(&session_id) && histories.len() >= MAX_SESSIONS {
                    if let Some(oldest) = histories
                        .iter()
                        .min_by_key(|(_, history)| history.last_used)
                        .map(|(id, _)| id.clone())
                    {
                        histories.remove(&oldest);
                    }
                }
                let history = histories
                    .entry(session_id)
                    .or_insert_with(|| SessionHistory {
                        messages: Vec::new(),
                        last_used: Instant::now(),
                    });
                history.last_used = Instant::now();
                history.messages.push(Message::user(req.message));
                history.messages.push(Message::assistant(response.clone()));
                if history.messages.len() > MAX_SESSION_MESSAGES {
                    history
                        .messages
                        .drain(..history.messages.len() - MAX_SESSION_MESSAGES);
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
            .into_response()
        }
        Ok(Err(e)) => (
            StatusCode::BAD_GATEWAY,
            Json(ChatResponse {
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
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(ChatResponse {
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
        )
            .into_response(),
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

fn valid_request_id(request_id: &str) -> bool {
    !request_id.is_empty()
        && request_id.len() <= MAX_REQUEST_ID_CHARS
        && request_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
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
    crate::tools::graph::ensure_ready()
        .await
        .map_err(anyhow::Error::msg)?;
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
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
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

    #[test]
    fn api_key_comparison_is_length_and_content_sensitive() {
        assert!(constant_time_eq(b"correct-secret", b"correct-secret"));
        assert!(!constant_time_eq(b"correct-secret", b"wrong-secret"));
        assert!(!constant_time_eq(
            b"correct-secret",
            b"correct-secret-extra"
        ));
    }

    #[test]
    fn request_ids_are_bounded_and_header_safe() {
        assert!(valid_request_id("deploy-42:attempt_1"));
        assert!(!valid_request_id("bad\nheader"));
        assert!(!valid_request_id(&"x".repeat(MAX_REQUEST_ID_CHARS + 1)));
    }
}
