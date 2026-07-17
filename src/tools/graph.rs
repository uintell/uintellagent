// Graph Memory Tools — SurrealDB persistent agent memory via REST API
//
// Credentials from env: UINTELL_DB_URL, UINTELL_DB_USER, UINTELL_DB_PASS
// Defaults: http://127.0.0.1:8000, root/root
//
// Tools: graph_store, graph_query, graph_context, graph_edit, graph_forget
// Provenance: every fact has source, confidence, timestamp, and tool_origin

use rig_core::completion::ToolDefinition;
use rig_core::tool::Tool;
use serde::Deserialize;
use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static SURREAL_STARTING: AtomicBool = AtomicBool::new(false);

const SCHEMA_SQL: &str = r#"
    DEFINE TABLE IF NOT EXISTS fact SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS fact_type ON fact TYPE string;
    DEFINE FIELD IF NOT EXISTS content ON fact TYPE string;
    DEFINE FIELD IF NOT EXISTS source ON fact TYPE string;
    DEFINE FIELD IF NOT EXISTS confidence ON fact TYPE float;
    DEFINE FIELD IF NOT EXISTS tags ON fact TYPE array;
    DEFINE FIELD IF NOT EXISTS tool_origin ON fact TYPE string;
    DEFINE FIELD IF NOT EXISTS timestamp ON fact TYPE datetime;
    DEFINE FIELD IF NOT EXISTS updated_at ON fact TYPE datetime;
    DEFINE FIELD IF NOT EXISTS dataset ON fact TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS graph_x ON fact TYPE option<float>;
    DEFINE FIELD IF NOT EXISTS graph_y ON fact TYPE option<float>;
    DEFINE FIELD IF NOT EXISTS graph_pinned ON fact TYPE option<bool>;
    DEFINE FIELD IF NOT EXISTS code_path ON fact TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS code_start_line ON fact TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS code_end_line ON fact TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS code_column ON fact TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS code_symbol ON fact TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS run_id ON fact TYPE option<string>;
    DEFINE INDEX IF NOT EXISTS fact_type_idx ON fact COLUMNS fact_type;
    DEFINE INDEX IF NOT EXISTS confidence_idx ON fact COLUMNS confidence;
    DEFINE INDEX IF NOT EXISTS dataset_idx ON fact COLUMNS dataset;
    DEFINE TABLE IF NOT EXISTS dataset SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS name ON dataset TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON dataset TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS created_at ON dataset TYPE datetime;
    DEFINE INDEX IF NOT EXISTS dataset_name_unique ON dataset FIELDS name UNIQUE;
    DEFINE TABLE IF NOT EXISTS relates_to TYPE RELATION IN fact OUT fact;
    DEFINE TABLE IF NOT EXISTS proves TYPE RELATION IN fact OUT fact;
    DEFINE TABLE IF NOT EXISTS graph_audit SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS action ON graph_audit TYPE string;
    DEFINE FIELD IF NOT EXISTS entity_id ON graph_audit TYPE string;
    DEFINE FIELD IF NOT EXISTS payload ON graph_audit TYPE string;
    DEFINE FIELD IF NOT EXISTS timestamp ON graph_audit TYPE datetime;
    DEFINE FIELD IF NOT EXISTS undone ON graph_audit TYPE bool;
    DEFINE INDEX IF NOT EXISTS graph_audit_time ON graph_audit COLUMNS timestamp;
"#;

fn db_url() -> String {
    std::env::var("UINTELL_DB_URL").unwrap_or_else(|_| "http://127.0.0.1:8000".into())
}
fn db_user() -> String {
    std::env::var("UINTELL_DB_USER").unwrap_or_else(|_| "root".into())
}
fn db_pass() -> String {
    std::env::var("UINTELL_DB_PASS").unwrap_or_else(|_| "root".into())
}

// ── REST helpers ────────────────────────────────────────────────

async fn db_query(sql: &str) -> Result<Vec<Value>, String> {
    match raw_db_query(sql).await {
        Ok(rows) => Ok(rows),
        Err(first_err) if should_autostart_surreal() => {
            ensure_local_surrealdb().await?;
            raw_db_query(sql).await.map_err(|second_err| {
                format!("after auto-start: {second_err}; original error: {first_err}")
            })
        }
        Err(err) => Err(err),
    }
}

async fn raw_db_query(sql: &str) -> Result<Vec<Value>, String> {
    let client = reqwest::Client::new();
    let body = format!("USE NS agent DB graph;\n{sql}");
    let resp = client
        .post(format!("{}/sql", db_url()))
        .header("Accept", "application/json")
        .header("NS", "agent")
        .header("DB", "graph")
        .header("Surreal-NS", "agent")
        .header("Surreal-DB", "graph")
        .basic_auth(db_user(), Some(db_pass()))
        .body(body)
        .send()
        .await
        .map_err(|e| format!("DB: {e}"))?;
    let body = resp.text().await.map_err(|e| format!("Read: {e}"))?;
    parse_surreal_response(&body)
}

fn parse_surreal_response(body: &str) -> Result<Vec<Value>, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|e| format!("Parse: {e} — {:.100}", body))?;

    match value {
        Value::Array(rows) => {
            if let Some(err) = rows.iter().find(|row| row["status"] == "ERR") {
                let msg = err["result"].as_str().unwrap_or("unknown SurrealDB error");
                return Err(msg.to_string());
            }
            Ok(rows
                .into_iter()
                .filter(|row| {
                    row.get("result")
                        .is_some_and(|result| !result.is_null() && !is_use_result(result))
                })
                .collect())
        }
        Value::Object(map) => {
            let description = map
                .get("description")
                .and_then(Value::as_str)
                .or_else(|| map.get("details").and_then(Value::as_str))
                .unwrap_or("SurrealDB request failed");
            Err(description.to_string())
        }
        other => Err(format!("Unexpected SurrealDB response: {other}")),
    }
}

fn is_use_result(result: &Value) -> bool {
    result.as_object().is_some_and(|object| {
        object.contains_key("namespace") && object.contains_key("database") && object.len() == 2
    })
}

/// Execute SurrealQL through the same authenticated, auto-starting path used by
/// the graph tools. The TUI uses this instead of maintaining a second client.
pub(crate) async fn query_sql(sql: &str) -> Result<Vec<Value>, String> {
    db_query(sql).await
}

pub(crate) struct CodeLocation<'a> {
    pub path: &'a str,
    pub start_line: usize,
    pub end_line: usize,
    pub column: usize,
    pub symbol: Option<&'a str>,
    pub snippet: &'a str,
    pub run_id: Option<&'a str>,
    pub related_fact_id: Option<&'a str>,
}

pub(crate) async fn store_code_location(location: CodeLocation<'_>) -> Result<String, String> {
    if let Some(fact_id) = location.related_fact_id {
        if !valid_record_id(fact_id) {
            return Err("related knowledge unit has an invalid record id".into());
        }
    }
    db_query(
        "UPSERT dataset:code SET name = 'code', description = 'Navigable source files, symbols, and agent run context', created_at = time::now()",
    )
    .await?;
    let label = location.symbol.unwrap_or("code location");
    let snippet = location.snippet.chars().take(1_500).collect::<String>();
    let content = format!(
        "{label} · {}:{}-{}\n{}",
        location.path, location.start_line, location.end_line, snippet
    );
    let extension = PathBuf::from(location.path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("text")
        .to_string();
    let sql = format!(
        "CREATE fact CONTENT {{ fact_type: 'code_location', content: {}, source: 'editor', confidence: 1.0, tags: ['code', {}], dataset: 'code', tool_origin: 'editor_memory', timestamp: time::now(), updated_at: time::now(), code_path: {}, code_start_line: {}, code_end_line: {}, code_column: {}, code_symbol: {}, run_id: {} }}",
        sql_string(&content),
        sql_string(&extension),
        sql_string(location.path),
        location.start_line,
        location.end_line,
        location.column,
        location
            .symbol
            .map(sql_string)
            .unwrap_or_else(|| "NONE".into()),
        location
            .run_id
            .map(sql_string)
            .unwrap_or_else(|| "NONE".into()),
    );
    let rows = db_query(&sql).await?;
    let id = rows
        .iter()
        .filter_map(|row| row.get("result"))
        .filter_map(Value::as_array)
        .flatten()
        .find_map(|row| row.get("id").and_then(Value::as_str))
        .ok_or_else(|| "SurrealDB did not return the code-location id".to_string())?
        .to_string();
    if let Some(related) = location.related_fact_id {
        db_query(&format!("RELATE {id}->relates_to->{related}")).await?;
    }
    Ok(id)
}

fn should_autostart_surreal() -> bool {
    if std::env::var("UINTELL_DB_AUTOSTART")
        .map(|v| matches!(v.as_str(), "0" | "false" | "no" | "off"))
        .unwrap_or(false)
    {
        return false;
    }

    matches!(
        db_url().as_str(),
        "http://127.0.0.1:8000" | "http://localhost:8000"
    )
}

async fn ensure_local_surrealdb() -> Result<(), String> {
    if raw_db_query("RETURN true").await.is_ok() {
        SURREAL_STARTING.store(false, Ordering::SeqCst);
        return Ok(());
    }

    if !SURREAL_STARTING.swap(true, Ordering::SeqCst) {
        if let Err(error) = spawn_local_surrealdb() {
            SURREAL_STARTING.store(false, Ordering::SeqCst);
            return Err(error);
        }
    }

    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if raw_db_query("RETURN true").await.is_ok() {
            SURREAL_STARTING.store(false, Ordering::SeqCst);
            return Ok(());
        }
    }

    SURREAL_STARTING.store(false, Ordering::SeqCst);
    Err(format!(
        "SurrealDB auto-started but did not become ready at {}",
        db_url()
    ))
}

pub async fn ensure_ready() -> Result<(), String> {
    if should_autostart_surreal() {
        ensure_local_surrealdb().await?;
    }

    let results = raw_db_query(SCHEMA_SQL).await?;
    for row in &results {
        if row["status"] == "ERR" {
            let msg = row["result"].as_str().unwrap_or("unknown schema error");
            return Err(format!("schema init failed: {msg}"));
        }
    }
    Ok(())
}

fn spawn_local_surrealdb() -> Result<(), String> {
    let data_path = surreal_data_path()?;
    std::fs::create_dir_all(&data_path).map_err(|e| format!("create db dir: {e}"))?;

    let log_path = uintell_home()?.join("surrealdb.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("open SurrealDB log: {e}"))?;
    let log_file_err = log_file
        .try_clone()
        .map_err(|e| format!("clone SurrealDB log handle: {e}"))?;

    let datastore = format!("surrealkv://{}", data_path.display());
    let mut command = std::process::Command::new("surreal");
    command
        .arg("start")
        .arg("--no-banner")
        .arg("--log")
        .arg("warn")
        .arg("--user")
        .arg(db_user())
        .arg("--pass")
        .arg(db_pass())
        .arg("--bind")
        .arg("127.0.0.1:8000")
        .arg(datastore)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err));

    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    command.spawn().map_err(|e| {
        format!(
            "start SurrealDB: {e}. Install it or set UINTELL_DB_AUTOSTART=0. Log: {}",
            log_path.display()
        )
    })?;

    Ok(())
}

fn uintell_home() -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|e| format!("HOME not set: {e}"))?;
    let path = PathBuf::from(home).join(".uintell");
    std::fs::create_dir_all(&path).map_err(|e| format!("create ~/.uintell: {e}"))?;
    Ok(path)
}

fn surreal_data_path() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("UINTELL_DB_PATH") {
        return Ok(PathBuf::from(path));
    }
    Ok(uintell_home()?.join("surrealdb"))
}

fn db_unavailable(action: &str, err: &str) -> String {
    format!(
        "Graph memory unavailable while trying to {action}: {err}. \
         Continue without memory, or check ~/.uintell/surrealdb.log."
    )
}

// ── Graph Store ─────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct GraphStoreArgs {
    fact_type: String,
    content: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    dataset: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("graph store error")]
pub struct GraphStoreError;

pub struct GraphStore;

impl Tool for GraphStore {
    const NAME: &'static str = "graph_store";
    type Error = GraphStoreError;
    type Args = GraphStoreArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "graph_store".to_string(),
            description: "Store a fact in persistent memory. Types: memory, preference, finding, user_detail, decision, error, fix. Confidence 0.0-1.0. Optional tags for categorization.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "fact_type": { "type": "string", "description": "Category" },
                    "content": { "type": "string", "description": "The fact text" },
                    "source": { "type": "string", "description": "Origin (tool name or 'agent')" },
                    "confidence": { "type": "number", "description": "Confidence 0.0-1.0 (default 0.8)" },
                    "tags": { "type": "array", "items": {"type": "string"}, "description": "Tags for categorization" },
                    "dataset": { "type": "string", "description": "Knowledge dataset (default: default)" }
                },
                "required": ["fact_type", "content"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let permission_args =
            json!({ "fact_type": &args.fact_type, "content": &args.content }).to_string();
        if let Err(reason) = crate::permissions::enforce_tool_call(Self::NAME, &permission_args) {
            return Ok(reason);
        }
        if !valid_label(&args.fact_type) {
            return Ok(
                "Invalid fact_type. Use 1-64 ASCII letters, digits, '.', '_' or '-'.".into(),
            );
        }
        if let Some(tags) = &args.tags {
            if !tags.iter().all(|tag| valid_label(tag)) {
                return Ok("Invalid tag. Use 1-64 ASCII letters, digits, '.', '_' or '-'.".into());
            }
        }
        let dataset = args.dataset.unwrap_or_else(|| "default".into());
        if !valid_label(&dataset) {
            return Ok("Invalid dataset. Use 1-64 ASCII letters, digits, '.', '_' or '-'.".into());
        }

        let confidence = confidence_value(args.confidence, 0.8);
        let source = args.source.unwrap_or_else(|| "agent".into());
        let tags = args
            .tags
            .map(|t| sql_string_array(&t))
            .unwrap_or_else(|| "[]".into());

        let sql = format!(
            "CREATE fact CONTENT {{ fact_type: {}, content: {}, source: {}, confidence: {}, tags: {tags}, dataset: {}, tool_origin: \"graph_store\", timestamp: time::now(), updated_at: time::now() }}",
            sql_string(&args.fact_type), sql_string(&args.content), sql_string(&source), confidence, sql_string(&dataset),
        );

        if let Err(err) = db_query(&sql).await {
            return Ok(db_unavailable("store a memory", &err));
        }
        Ok(format!(
            "Stored: [{}] {}",
            args.fact_type,
            trunc(&args.content, 80)
        ))
    }
}

// ── Graph Query ─────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct GraphQueryArgs {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    fact_types: Option<Vec<String>>,
    #[serde(default)]
    min_confidence: Option<f32>,
}

#[derive(Debug, thiserror::Error)]
#[error("graph query error")]
pub struct GraphQueryError;

pub struct GraphQuery;

impl Tool for GraphQuery {
    const NAME: &'static str = "graph_query";
    type Error = GraphQueryError;
    type Args = GraphQueryArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "graph_query".to_string(),
            description: "Search memory for facts. Returns results ranked by confidence × recency. Filter by fact_type and minimum confidence.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search term" },
                    "limit": { "type": "integer", "description": "Max results (default 10)" },
                    "fact_types": { "type": "array", "items": {"type": "string"}, "description": "Filter by types" },
                    "min_confidence": { "type": "number", "description": "Minimum confidence 0.0-1.0" }
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

        let limit = bounded_limit(args.limit, 10, 50);
        let min_conf = confidence_value(args.min_confidence, 0.0);

        let type_filter = match &args.fact_types {
            Some(types) if !types.is_empty() => {
                if !types.iter().all(|fact_type| valid_label(fact_type)) {
                    return Ok("Invalid fact_type filter.".into());
                }
                let list = types
                    .iter()
                    .map(|t| sql_string(t))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("AND fact_type IN [{list}]")
            }
            _ => String::new(),
        };

        let sql = format!(
            "SELECT id, fact_type, content, source, confidence, tags, timestamp FROM fact WHERE content CONTAINS {} {type_filter} AND confidence >= {min_conf} ORDER BY confidence DESC, timestamp DESC LIMIT {limit}",
            sql_string(&args.query),
        );

        let rows = match db_query(&sql).await {
            Ok(rows) => rows,
            Err(err) => return Ok(db_unavailable("query memory", &err)),
        };
        let result = rows.first().and_then(|r| r["result"].as_array());

        match result {
            Some(items) if !items.is_empty() => {
                let mut out = String::new();
                for (i, row) in items.iter().enumerate() {
                    let id = row["id"].as_str().unwrap_or("?");
                    let ft = row["fact_type"].as_str().unwrap_or("?");
                    let c = row["content"].as_str().unwrap_or("?");
                    let conf = row["confidence"].as_f64().unwrap_or(0.0);
                    let tags = row["tags"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    let tag_str = if tags.is_empty() {
                        String::new()
                    } else {
                        format!(" [{tags}]")
                    };
                    out.push_str(&format!(
                        "{}. {id} [{ft}]{tag_str} {c} (conf: {conf:.1})\n",
                        i + 1
                    ));
                }
                Ok(out)
            }
            _ => Ok("No matching memories found.".into()),
        }
    }
}

// ── Graph Context ───────────────────────────────────────────────

#[derive(Deserialize)]
pub struct GraphContextArgs {
    #[serde(default)]
    fact_types: Option<Vec<String>>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    min_confidence: Option<f32>,
}

#[derive(Debug, thiserror::Error)]
#[error("graph context error")]
pub struct GraphContextError;

pub struct GraphContext;

impl Tool for GraphContext {
    const NAME: &'static str = "graph_context";
    type Error = GraphContextError;
    type Args = GraphContextArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "graph_context".to_string(),
            description: "Load relevant context from memory. Call at conversation start to recall user preferences, project facts, and past decisions. Results ranked by confidence × recency.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "fact_types": { "type": "array", "items": {"type": "string"}, "description": "Filter by types" },
                    "limit": { "type": "integer", "description": "Max results (default 20)" },
                    "min_confidence": { "type": "number", "description": "Minimum confidence" }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let permission_args = json!({}).to_string();
        if let Err(reason) = crate::permissions::enforce_tool_call(Self::NAME, &permission_args) {
            return Ok(reason);
        }

        let limit = bounded_limit(args.limit, 20, 100);
        let min_conf = confidence_value(args.min_confidence, 0.3);

        let type_filter = match &args.fact_types {
            Some(types) if !types.is_empty() => {
                if !types.iter().all(|fact_type| valid_label(fact_type)) {
                    return Ok("Invalid fact_type filter.".into());
                }
                let list = types
                    .iter()
                    .map(|t| sql_string(t))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("WHERE fact_type IN [{list}] AND confidence >= {min_conf}")
            }
            _ => format!("WHERE confidence >= {min_conf}"),
        };

        let sql = format!("SELECT fact_type, content, confidence, tags, timestamp FROM fact {type_filter} ORDER BY confidence DESC, timestamp DESC LIMIT {limit}");
        let rows = match db_query(&sql).await {
            Ok(rows) => rows,
            Err(err) => return Ok(db_unavailable("load context", &err)),
        };
        let items = rows.first().and_then(|r| r["result"].as_array());

        match items {
            Some(arr) if !arr.is_empty() => {
                let mut out = String::from("--- Graph Memory Context ---\n");
                for row in arr {
                    let ft = row["fact_type"].as_str().unwrap_or("?");
                    let c = row["content"].as_str().unwrap_or("?");
                    let conf = row["confidence"].as_f64().unwrap_or(0.0);
                    out.push_str(&format!("[{ft}] {c} (conf: {conf:.1})\n"));
                }
                out.push_str("--- End Context ---");
                Ok(out)
            }
            _ => Ok("No context found.".into()),
        }
    }
}

// ── Graph Edit ──────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct GraphEditArgs {
    fact_id: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    fact_type: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("graph edit error")]
pub struct GraphEditError;

pub struct GraphEdit;

impl Tool for GraphEdit {
    const NAME: &'static str = "graph_edit";
    type Error = GraphEditError;
    type Args = GraphEditArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "graph_edit".to_string(),
            description: "Update an existing memory fact. Provide the fact ID and fields to change. Use graph_query to find the ID first.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "fact_id": { "type": "string", "description": "Fact record ID (from graph_query)" },
                    "content": { "type": "string", "description": "New content" },
                    "confidence": { "type": "number", "description": "New confidence 0.0-1.0" },
                    "fact_type": { "type": "string", "description": "New category" }
                },
                "required": ["fact_id"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        if !valid_record_id(&args.fact_id) {
            return Ok("Invalid fact_id. Expected a SurrealDB record id like fact:abc123.".into());
        }
        let permission_args = json!({ "fact_id": &args.fact_id }).to_string();
        if let Err(reason) = crate::permissions::enforce_tool_call(Self::NAME, &permission_args) {
            return Ok(reason);
        }

        let mut sets = Vec::new();
        if let Some(ref c) = args.content {
            sets.push(format!("content = {}", sql_string(c)));
        }
        if let Some(c) = args.confidence {
            sets.push(format!("confidence = {}", confidence_value(Some(c), 0.8)));
        }
        if let Some(ref t) = args.fact_type {
            if !valid_label(t) {
                return Ok(
                    "Invalid fact_type. Use 1-64 ASCII letters, digits, '.', '_' or '-'.".into(),
                );
            }
            sets.push(format!("fact_type = {}", sql_string(t)));
        }
        sets.push("updated_at = time::now()".into());

        if sets.is_empty() {
            return Ok("No fields to update.".into());
        }

        let sql = format!(
            "UPDATE {} SET {} RETURN AFTER",
            args.fact_id,
            sets.join(", ")
        );
        if let Err(err) = db_query(&sql).await {
            return Ok(db_unavailable("edit memory", &err));
        }
        Ok(format!("Updated fact {}", args.fact_id))
    }
}

// ── Graph Forget ────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct GraphForgetArgs {
    fact_id: String,
}

#[derive(Debug, thiserror::Error)]
#[error("graph forget error")]
pub struct GraphForgetError;

pub struct GraphForget;

impl Tool for GraphForget {
    const NAME: &'static str = "graph_forget";
    type Error = GraphForgetError;
    type Args = GraphForgetArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "graph_forget".to_string(),
            description: "Delete a memory fact permanently. Use graph_query to find the ID first. This cannot be undone.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "fact_id": { "type": "string", "description": "Fact record ID to delete" }
                },
                "required": ["fact_id"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        if !valid_record_id(&args.fact_id) {
            return Ok("Invalid fact_id. Expected a SurrealDB record id like fact:abc123.".into());
        }
        let permission_args = json!({ "fact_id": &args.fact_id }).to_string();
        if let Err(reason) = crate::permissions::enforce_tool_call(Self::NAME, &permission_args) {
            return Ok(reason);
        }

        let sql = format!("DELETE {}", args.fact_id);
        if let Err(err) = db_query(&sql).await {
            return Ok(db_unavailable("forget memory", &err));
        }
        Ok(format!("Forgot fact {}", args.fact_id))
    }
}

// ── Schema Init ─────────────────────────────────────────────────

pub async fn init_schema() -> anyhow::Result<()> {
    ensure_ready().await.map_err(|e| anyhow::anyhow!("{e}"))
}

fn trunc(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

fn valid_record_id(id: &str) -> bool {
    let mut parts = id.split(':');
    let Some(table) = parts.next() else {
        return false;
    };
    let Some(record) = parts.next() else {
        return false;
    };
    if parts.next().is_some() || table.is_empty() || record.is_empty() {
        return false;
    }
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-' | '.'))
}

fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn confidence_value(value: Option<f32>, default: f32) -> f32 {
    let value = value.unwrap_or(default);
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        default
    }
}

fn bounded_limit(limit: Option<usize>, default: usize, max: usize) -> usize {
    limit.unwrap_or(default).clamp(1, max)
}

fn sql_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

fn sql_string_array(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|value| sql_string(value))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_id_validation_rejects_sql_fragments() {
        assert!(valid_record_id("fact:abc_123"));
        assert!(!valid_record_id("fact:abc; DELETE fact"));
        assert!(!valid_record_id("fact"));
        assert!(!valid_record_id("fact:"));
    }

    #[test]
    fn sql_string_serializes_user_text() {
        assert_eq!(sql_string("a'b\\c"), "\"a'b\\\\c\"");
        assert_eq!(
            sql_string("x\"; DELETE fact; --"),
            "\"x\\\"; DELETE fact; --\""
        );
    }

    #[test]
    fn labels_are_strict() {
        assert!(valid_label("user_detail"));
        assert!(valid_label("finding-1"));
        assert!(!valid_label("bad tag"));
        assert!(!valid_label("x; DELETE fact"));
    }

    #[test]
    fn surreal_response_drops_use_statement_but_keeps_query_rows() {
        let body = r#"[
            {"status":"OK","result":{"namespace":"agent","database":"graph"}},
            {"status":"OK","result":[{"id":"fact:one","content":"remember me"}]}
        ]"#;

        let rows = parse_surreal_response(body).expect("valid response");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["result"][0]["id"], "fact:one");
    }

    #[test]
    fn surreal_response_surfaces_statement_errors() {
        let body = r#"[{"status":"ERR","result":"bad query"}]"#;
        assert_eq!(
            parse_surreal_response(body).expect_err("error response"),
            "bad query"
        );
    }
}
