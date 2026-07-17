//! Minimal asynchronous LSP client used by the integrated editor.
//!
//! The protocol worker owns a language-server child process and exposes typed
//! events to the TUI without blocking its render loop. Rust is supported first;
//! other language servers can be selected with `UINTELL_LSP_COMMAND`.

use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use url::Url;

const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    pub line: usize,
    pub character: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: u8,
    pub message: String,
    pub source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub detail: Option<String>,
    pub new_text: String,
    pub range: Option<Range>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub range: Range,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Ready {
        server: String,
    },
    Unavailable(String),
    Diagnostics {
        path: PathBuf,
        diagnostics: Vec<Diagnostic>,
    },
    Completion {
        path: PathBuf,
        items: Vec<CompletionItem>,
    },
    Definition(Option<Location>),
    Error(String),
}

#[derive(Clone, Debug)]
enum RequestKind {
    Initialize,
    Completion { path: PathBuf },
    Definition,
}

#[derive(Clone, Debug)]
enum WireEvent {
    Message(Value),
    Closed,
    Error(String),
}

#[derive(Clone, Debug)]
struct DocumentState {
    uri: String,
    text: String,
    version: i64,
    opened: bool,
}

pub struct Client {
    writer: Option<Arc<Mutex<ChildStdin>>>,
    child: Option<Child>,
    incoming: mpsc::Receiver<WireEvent>,
    pending: HashMap<i64, RequestKind>,
    documents: HashMap<PathBuf, DocumentState>,
    events: VecDeque<Event>,
    next_id: i64,
    ready: bool,
    root: PathBuf,
    root_uri: String,
    server_name: String,
    rust_only: bool,
}

impl Client {
    pub fn start(root: &Path) -> Self {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let root_uri = path_to_uri(&root).unwrap_or_else(|| "file:///".into());
        let command =
            std::env::var("UINTELL_LSP_COMMAND").unwrap_or_else(|_| "rust-analyzer".into());
        let arguments = std::env::var("UINTELL_LSP_ARGS")
            .ok()
            .map(|value| {
                value
                    .split_whitespace()
                    .map(String::from)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let server_name = Path::new(&command)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&command)
            .to_string();
        let rust_only = server_name.contains("rust-analyzer");
        let (_wire_tx, wire_rx) = mpsc::channel();
        let mut client = Self {
            writer: None,
            child: None,
            incoming: wire_rx,
            pending: HashMap::new(),
            documents: HashMap::new(),
            events: VecDeque::new(),
            next_id: 1,
            ready: false,
            root,
            root_uri,
            server_name,
            rust_only,
        };

        let mut child = match Command::new(&command)
            .args(arguments)
            .current_dir(&client.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                client.events.push_back(Event::Unavailable(format!(
                    "could not start {command}: {error}"
                )));
                return client;
            }
        };

        let Some(stdin) = child.stdin.take() else {
            client
                .events
                .push_back(Event::Unavailable(format!("{command} has no stdin")));
            let _ = child.kill();
            return client;
        };
        let Some(stdout) = child.stdout.take() else {
            client
                .events
                .push_back(Event::Unavailable(format!("{command} has no stdout")));
            let _ = child.kill();
            return client;
        };

        let writer = Arc::new(Mutex::new(stdin));
        let (wire_tx, wire_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("uintell-lsp-reader".into())
            .spawn(move || read_server(stdout, wire_tx))
            .ok();
        if let Some(stderr) = child.stderr.take() {
            std::thread::Builder::new()
                .name("uintell-lsp-stderr".into())
                .spawn(move || {
                    let mut reader = BufReader::new(stderr);
                    let mut sink = String::new();
                    let _ = reader.read_to_string(&mut sink);
                })
                .ok();
        }

        client.writer = Some(writer);
        client.child = Some(child);
        client.incoming = wire_rx;
        let initialize = json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "uintell-agent", "version": env!("CARGO_PKG_VERSION")},
            "rootUri": client.root_uri,
            "workspaceFolders": [{"uri": client.root_uri, "name": "workspace"}],
            "capabilities": {
                "workspace": {"configuration": true, "workspaceFolders": true},
                "textDocument": {
                    "synchronization": {"dynamicRegistration": false, "didSave": true},
                    "completion": {"completionItem": {"snippetSupport": false}},
                    "definition": {"linkSupport": true},
                    "publishDiagnostics": {"relatedInformation": true}
                }
            }
        });
        if let Err(error) = client.request("initialize", initialize, RequestKind::Initialize) {
            client
                .events
                .push_back(Event::Unavailable(format!("initialize failed: {error}")));
        }
        client
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    pub fn supports_path(&self, path: &Path) -> bool {
        !self.rust_only || path.extension().and_then(|extension| extension.to_str()) == Some("rs")
    }

    pub fn sync_document(&mut self, path: &Path, text: &str) -> Result<(), String> {
        if !self.supports_path(path) {
            return Err(format!("{} only handles Rust files", self.server_name));
        }
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let uri = path_to_uri(&path).ok_or_else(|| format!("invalid path: {}", path.display()))?;
        let state = self
            .documents
            .entry(path.clone())
            .or_insert_with(|| DocumentState {
                uri,
                text: String::new(),
                version: 0,
                opened: false,
            });
        if state.text == text && state.opened {
            return Ok(());
        }
        state.text = text.to_string();
        if !self.ready {
            return Ok(());
        }
        if !state.opened {
            state.version = 1;
            state.opened = true;
            let params = json!({
                "textDocument": {
                    "uri": state.uri,
                    "languageId": language_id(&path),
                    "version": state.version,
                    "text": state.text,
                }
            });
            self.notify("textDocument/didOpen", params)
        } else {
            state.version += 1;
            let params = json!({
                "textDocument": {"uri": state.uri, "version": state.version},
                "contentChanges": [{"text": state.text}],
            });
            self.notify("textDocument/didChange", params)
        }
    }

    pub fn close_document(&mut self, path: &Path) {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let Some(state) = self.documents.remove(&path) else {
            return;
        };
        if self.ready && state.opened {
            let _ = self.notify(
                "textDocument/didClose",
                json!({"textDocument": {"uri": state.uri}}),
            );
        }
    }

    pub fn request_completion(&mut self, path: &Path, position: Position) -> Result<(), String> {
        if !self.supports_path(path) {
            return Err(format!("{} only handles Rust files", self.server_name));
        }
        self.ensure_ready()?;
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let uri = self
            .documents
            .get(&path)
            .map(|state| state.uri.clone())
            .or_else(|| path_to_uri(&path))
            .ok_or_else(|| format!("invalid path: {}", path.display()))?;
        self.request(
            "textDocument/completion",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": position.line, "character": position.character},
                "context": {"triggerKind": 1},
            }),
            RequestKind::Completion { path },
        )
        .map(|_| ())
    }

    pub fn request_definition(&mut self, path: &Path, position: Position) -> Result<(), String> {
        if !self.supports_path(path) {
            return Err(format!("{} only handles Rust files", self.server_name));
        }
        self.ensure_ready()?;
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let uri = self
            .documents
            .get(&path)
            .map(|state| state.uri.clone())
            .or_else(|| path_to_uri(&path))
            .ok_or_else(|| format!("invalid path: {}", path.display()))?;
        self.request(
            "textDocument/definition",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": position.line, "character": position.character},
            }),
            RequestKind::Definition,
        )
        .map(|_| ())
    }

    pub fn poll(&mut self) -> Vec<Event> {
        while let Ok(event) = self.incoming.try_recv() {
            match event {
                WireEvent::Message(message) => self.handle_message(message),
                WireEvent::Closed => {
                    if self.ready {
                        self.events.push_back(Event::Error(format!(
                            "{} stopped unexpectedly",
                            self.server_name
                        )));
                    } else {
                        self.events.push_back(Event::Unavailable(format!(
                            "{} exited during initialization",
                            self.server_name
                        )));
                    }
                    self.ready = false;
                    self.writer = None;
                }
                WireEvent::Error(error) => self.events.push_back(Event::Error(error)),
            }
        }
        self.events.drain(..).collect()
    }

    fn ensure_ready(&self) -> Result<(), String> {
        if self.ready {
            Ok(())
        } else {
            Err(format!("{} is not ready", self.server_name))
        }
    }

    fn request(&mut self, method: &str, params: Value, kind: RequestKind) -> Result<i64, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        self.pending.insert(id, kind);
        Ok(id)
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    fn send(&self, message: Value) -> Result<(), String> {
        let writer = self
            .writer
            .as_ref()
            .ok_or_else(|| "language server is not running".to_string())?;
        let mut writer = writer
            .lock()
            .map_err(|_| "language server writer lock is poisoned".to_string())?;
        write_message(&mut *writer, &message).map_err(|error| error.to_string())
    }

    fn handle_message(&mut self, message: Value) {
        if message.get("method").is_some() {
            self.handle_server_method(message);
            return;
        }
        let Some(id) = message.get("id").and_then(Value::as_i64) else {
            return;
        };
        let Some(kind) = self.pending.remove(&id) else {
            return;
        };
        if let Some(error) = message.get("error") {
            self.events.push_back(Event::Error(format!(
                "LSP request failed: {}",
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            )));
            return;
        }
        let result = message.get("result").cloned().unwrap_or(Value::Null);
        match kind {
            RequestKind::Initialize => self.finish_initialize(&result),
            RequestKind::Completion { path } => {
                self.events.push_back(Event::Completion {
                    path,
                    items: parse_completion_items(&result),
                });
            }
            RequestKind::Definition => {
                self.events
                    .push_back(Event::Definition(parse_definition(&result)));
            }
        }
    }

    fn finish_initialize(&mut self, result: &Value) {
        self.ready = true;
        let advertised = result
            .pointer("/serverInfo/name")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(name) = advertised {
            self.server_name = name;
        }
        let _ = self.notify("initialized", json!({}));
        let _ = self.notify(
            "workspace/didChangeConfiguration",
            json!({"settings": {"rust-analyzer": {"check": {"command": "clippy"}}}}),
        );
        let queued = self
            .documents
            .iter()
            .map(|(path, state)| (path.clone(), state.text.clone()))
            .collect::<Vec<_>>();
        for (path, text) in queued {
            let _ = self.sync_document(&path, &text);
        }
        self.events.push_back(Event::Ready {
            server: self.server_name.clone(),
        });
    }

    fn handle_server_method(&mut self, message: Value) {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if method == "textDocument/publishDiagnostics" {
            let Some(uri) = message.pointer("/params/uri").and_then(Value::as_str) else {
                return;
            };
            let Some(path) = uri_to_path(uri) else {
                return;
            };
            let diagnostics = message
                .pointer("/params/diagnostics")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(parse_diagnostic).collect())
                .unwrap_or_default();
            self.events
                .push_back(Event::Diagnostics { path, diagnostics });
            return;
        }

        let Some(id) = message.get("id").cloned() else {
            return;
        };
        let result = match method {
            "workspace/configuration" => {
                let count = message
                    .pointer("/params/items")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                Value::Array((0..count).map(|_| json!({})).collect())
            }
            "workspace/workspaceFolders" => json!([{
                "uri": self.root_uri,
                "name": self.root.file_name().and_then(|name| name.to_str()).unwrap_or("workspace")
            }]),
            "client/registerCapability"
            | "client/unregisterCapability"
            | "window/workDoneProgress/create" => Value::Null,
            _ => {
                let _ = self.send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("unsupported client method: {method}")}
                }));
                return;
            }
        };
        let _ = self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.writer.is_some() {
            let id = self.next_id;
            let _ = self.send(json!({"jsonrpc": "2.0", "id": id, "method": "shutdown"}));
            let _ = self.send(json!({"jsonrpc": "2.0", "method": "exit"}));
        }
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn read_server(stdout: impl Read, sender: mpsc::Sender<WireEvent>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_message(&mut reader) {
            Ok(Some(message)) => {
                if sender.send(WireEvent::Message(message)).is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = sender.send(WireEvent::Closed);
                return;
            }
            Err(error) => {
                let _ = sender.send(WireEvent::Error(format!("LSP read error: {error}")));
                return;
            }
        }
    }
}

fn read_message(reader: &mut impl BufRead) -> std::io::Result<Option<Value>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some(value) = line
            .split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim())
        {
            content_length = value.parse::<usize>().ok();
        }
    }
    let length = content_length.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "missing Content-Length")
    })?;
    if length > MAX_MESSAGE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "language-server message is too large",
        ));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn write_message(writer: &mut impl Write, message: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

fn parse_position(value: &Value) -> Option<Position> {
    Some(Position {
        line: value.get("line")?.as_u64()? as usize,
        character: value.get("character")?.as_u64()? as usize,
    })
}

fn parse_range(value: &Value) -> Option<Range> {
    Some(Range {
        start: parse_position(value.get("start")?)?,
        end: parse_position(value.get("end")?)?,
    })
}

fn parse_diagnostic(value: &Value) -> Option<Diagnostic> {
    Some(Diagnostic {
        range: parse_range(value.get("range")?)?,
        severity: value.get("severity").and_then(Value::as_u64).unwrap_or(3) as u8,
        message: value.get("message")?.as_str()?.to_string(),
        source: value
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn parse_completion_items(result: &Value) -> Vec<CompletionItem> {
    let items = result
        .as_array()
        .or_else(|| result.get("items").and_then(Value::as_array));
    items
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let label = item.get("label")?.as_str()?.to_string();
            let text_edit = item.get("textEdit");
            let range = text_edit.and_then(|edit| {
                edit.get("range")
                    .or_else(|| edit.get("replace"))
                    .or_else(|| edit.get("insert"))
                    .and_then(parse_range)
            });
            let raw_text = text_edit
                .and_then(|edit| edit.get("newText"))
                .and_then(Value::as_str)
                .or_else(|| item.get("insertText").and_then(Value::as_str))
                .unwrap_or(&label);
            let new_text = if item.get("insertTextFormat").and_then(Value::as_u64) == Some(2) {
                strip_snippet_markers(raw_text)
            } else {
                raw_text.to_string()
            };
            let detail = item
                .get("detail")
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(CompletionItem {
                label,
                detail,
                new_text,
                range,
            })
        })
        .take(100)
        .collect()
}

fn parse_definition(result: &Value) -> Option<Location> {
    let item = result
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(result);
    let uri = item
        .get("uri")
        .or_else(|| item.get("targetUri"))?
        .as_str()?;
    let range = item
        .get("range")
        .or_else(|| item.get("targetSelectionRange"))
        .or_else(|| item.get("targetRange"))
        .and_then(parse_range)?;
    Some(Location {
        path: uri_to_path(uri)?,
        range,
    })
}

fn strip_snippet_markers(snippet: &str) -> String {
    let mut output = String::new();
    let bytes = snippet.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'$' {
            let character = snippet[index..].chars().next().unwrap_or_default();
            output.push(character);
            index += character.len_utf8();
            continue;
        }
        if bytes.get(index + 1) == Some(&b'{') {
            let Some(relative_end) = snippet[index + 2..].find('}') else {
                output.push('$');
                index += 1;
                continue;
            };
            let end = index + 2 + relative_end;
            let placeholder = &snippet[index + 2..end];
            if let Some((_, default)) = placeholder.split_once(':') {
                output.push_str(default);
            } else if let Some((_, choices)) = placeholder.split_once('|') {
                output.push_str(
                    choices
                        .trim_end_matches('|')
                        .split(',')
                        .next()
                        .unwrap_or_default(),
                );
            }
            index = end + 1;
            continue;
        }
        let mut end = index + 1;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        if end > index + 1 {
            index = end;
        } else {
            output.push('$');
            index += 1;
        }
    }
    output
}

fn path_to_uri(path: &Path) -> Option<String> {
    Url::from_file_path(path).ok().map(String::from)
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    Url::parse(uri).ok()?.to_file_path().ok()
}

fn language_id(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("rs") => "rust",
        Some("py") => "python",
        Some("js" | "mjs" | "cjs") => "javascript",
        Some("ts" | "mts" | "cts") => "typescript",
        Some("tsx") => "typescriptreact",
        Some("jsx") => "javascriptreact",
        Some("go") => "go",
        Some("c" | "h") => "c",
        Some("cc" | "cpp" | "cxx" | "hpp") => "cpp",
        Some("json") => "json",
        Some("toml") => "toml",
        Some("yaml" | "yml") => "yaml",
        Some("md") => "markdown",
        _ => "plaintext",
    }
}

pub fn utf16_column(line: &str, byte_column: usize) -> usize {
    let mut column = byte_column.min(line.len());
    while column > 0 && !line.is_char_boundary(column) {
        column -= 1;
    }
    line[..column].encode_utf16().count()
}

pub fn byte_column(line: &str, utf16_column: usize) -> usize {
    let mut units = 0;
    for (offset, character) in line.char_indices() {
        let next = units + character.len_utf16();
        if next > utf16_column {
            return offset;
        }
        units = next;
    }
    line.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_and_byte_columns_round_trip_unicode() {
        let line = "a😀éz";
        for byte in [0, 1, 5, 7, 8] {
            assert_eq!(byte_column(line, utf16_column(line, byte)), byte);
        }
    }

    #[test]
    fn snippet_markers_are_flattened_for_plain_text_editors() {
        assert_eq!(
            strip_snippet_markers("println!(\"${1:value}\");$0"),
            "println!(\"value\");"
        );
        assert_eq!(strip_snippet_markers("${1|one,two|}"), "one");
    }

    #[test]
    fn completion_parser_prefers_text_edit() {
        let items = parse_completion_items(&json!({"items": [{
            "label": "print",
            "detail": "macro",
            "insertTextFormat": 2,
            "textEdit": {
                "range": {
                    "start": {"line": 2, "character": 3},
                    "end": {"line": 2, "character": 5}
                },
                "newText": "println!($0)"
            }
        }]}));
        assert_eq!(items[0].new_text, "println!() ".trim_end());
        assert_eq!(items[0].range.unwrap().start.line, 2);
    }

    #[test]
    fn content_length_framing_round_trips() {
        let message = json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}});
        let mut bytes = Vec::new();
        write_message(&mut bytes, &message).unwrap();
        let decoded = read_message(&mut BufReader::new(bytes.as_slice()))
            .unwrap()
            .unwrap();
        assert_eq!(decoded, message);
    }
}
