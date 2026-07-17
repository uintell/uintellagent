// UIntell Unified TUI — Chat, Memory, Tools, Editor, and Runs in one terminal
//
// Tabs:
//   Chat   — agent conversation (streaming, markdown, tools)
//   Memory — interactive SurrealDB knowledge graph and dataset manager
//   Tools  — inspect and manually execute every registered agent tool
//   Editor — inspect and edit workspace files, including agent-authored changes
//   Runs   — durable autonomous coding runs with verification and review gates
//
// Keys:
//   Alt+1/2/3/4/5 — switch agent workspaces
//   Chat:  Alt+Enter send, Enter newline, /commands
//   Memory: j/k select, arrows/mouse move, f dataset, l link, :help
//   Tools:  j/k nav, Enter prepares call, :run <tool> <json>
//   Editor: Tab changes tree/editor focus, Ctrl+Space completes a word

use crate::confirm::{self, ConfirmState};
use crate::db_tui::{GraphConsole, GraphConsoleAction, GraphConsoleState};
use crate::editor::{self, Editor};
use crate::lsp;
use crate::provider_health::ProviderHealth;
use crate::task_run::{
    TaskNotification, TaskRun, TaskRunStatus, TaskRunSummary, TaskStore, TaskView,
};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Tabs, Wrap,
    },
    DefaultTerminal,
};
use rig_core::agent::{Agent, MultiTurnStreamItem};
use rig_core::completion::{CompletionModel, Message as RigMessage};
use rig_core::streaming::{
    StreamedAssistantContent, StreamedUserContent, StreamingPrompt, ToolCallDeltaContent,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};
use tokio::task::{AbortHandle, JoinHandle};
use tui_textarea::CursorMove;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

// ═══════════════════════════════════════════════════════════════
// THEME
// ═══════════════════════════════════════════════════════════════

const GREEN: Color = Color::Rgb(0x33, 0xFF, 0x33);
const DIM_GREEN: Color = Color::Rgb(0x1A, 0x7A, 0x1A);
const BG: Color = Color::Rgb(0x05, 0x05, 0x05);
const CYAN: Color = Color::Rgb(0x00, 0xCC, 0xCC);
const RED: Color = Color::Rgb(0xFF, 0x33, 0x33);
const YELLOW: Color = Color::Rgb(0xCC, 0xCC, 0x00);
const MAGENTA: Color = Color::Rgb(0xCC, 0x00, 0xCC);
const GRAY: Color = Color::Rgb(0x66, 0x66, 0x66);
const DARK_GRAY: Color = Color::Rgb(0x33, 0x33, 0x33);
const WHITE: Color = Color::Rgb(0xCC, 0xCC, 0xCC);

// ═══════════════════════════════════════════════════════════════
// STREAM EVENTS
// ═══════════════════════════════════════════════════════════════

enum StreamEvent {
    TextDelta(String),
    ToolCall {
        name: String,
    },
    ToolArgs(String),
    ToolResult {
        name: String,
        result: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    Done(String),
    Error(String),
}

// ═══════════════════════════════════════════════════════════════
// MESSAGE / FACT TYPES
// ═══════════════════════════════════════════════════════════════

#[derive(Clone)]
enum MsgKind {
    User,
    Agent,
    System,
    Error,
    ToolCall { name: String },
    ToolResult { name: String },
}

#[derive(Clone)]
struct Message {
    kind: MsgKind,
    text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Chat,
    Memory,
    Tools,
    Editor,
    Runs,
}

fn workspace_shortcut(current: Tab, active_view_accepts_text: bool, key: KeyEvent) -> Option<Tab> {
    let plain_shortcut =
        key.modifiers == KeyModifiers::NONE && current != Tab::Memory && !active_view_accepts_text;
    let global_shortcut = key.modifiers == KeyModifiers::ALT;
    if !plain_shortcut && !global_shortcut {
        return None;
    }
    match key.code {
        KeyCode::Char('1') => Some(Tab::Chat),
        KeyCode::Char('2') => Some(Tab::Memory),
        KeyCode::Char('3') => Some(Tab::Tools),
        KeyCode::Char('4') => Some(Tab::Editor),
        KeyCode::Char('5') => Some(Tab::Runs),
        _ => None,
    }
}

fn command_modifiers_accept_text(modifiers: KeyModifiers) -> bool {
    matches!(modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
}

fn chat_mode_shortcut(key: KeyEvent) -> Option<Mode> {
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('f')) => Some(Mode::Search),
        _ => None,
    }
}

impl Tab {
    const ALL: [Tab; 5] = [Tab::Chat, Tab::Memory, Tab::Tools, Tab::Editor, Tab::Runs];
    fn title(&self) -> &str {
        match self {
            Tab::Chat => "Chat",
            Tab::Memory => "Memory",
            Tab::Tools => "Tools",
            Tab::Editor => "Editor",
            Tab::Runs => "Runs",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    Command,
    Search,
    Palette,
}

#[derive(Clone, Copy)]
enum PaletteAction {
    SwitchTab(Tab),
    MemoryView(&'static str),
    RefreshMemory,
    BrowseEditor,
    SaveEditor,
    EditorToChat,
    EditorToRun,
    EditorPinMemory,
    EditorDefinition,
    ReviewAcceptAll,
    ReviewRejectAll,
    ReviewUndo,
    NewTask,
    ResumeTask,
    CancelTask,
    SelectTool(usize),
}

#[derive(Clone)]
struct PaletteItem {
    title: String,
    detail: String,
    action: PaletteAction,
}

struct ActiveTask {
    id: String,
    cancel: watch::Sender<bool>,
    join: JoinHandle<()>,
}

fn chat_input(lines: Vec<String>) -> tui_textarea::TextArea<'static> {
    let mut input = if lines.is_empty() {
        tui_textarea::TextArea::default()
    } else {
        tui_textarea::TextArea::new(lines)
    };
    input.set_block(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(DIM_GREEN))
            .bg(BG),
    );
    input.set_style(Style::default().fg(GREEN).bg(BG));
    input.set_cursor_style(Style::default().fg(BG).bg(GREEN));
    input.set_placeholder_style(Style::default().fg(DARK_GRAY));
    input
}

// ═══════════════════════════════════════════════════════════════
// APP STATE
// ═══════════════════════════════════════════════════════════════

struct App<M: CompletionModel + 'static> {
    // Tabs
    tab: Tab,
    // Chat
    messages: VecDeque<Message>,
    input: tui_textarea::TextArea<'static>,
    thinking: bool,
    streaming_text: String,
    active_tool: Option<String>,
    tool_args: String,
    active_run: Option<AbortHandle>,
    agent: Arc<Agent<M>>,
    provider_label: String,
    provider_health: ProviderHealth,
    chat_history: Vec<RigMessage>,
    // Memory
    memory_console: GraphConsole,
    // Tools
    tool_selected: usize,
    tool_output: String,
    // Shared
    scroll: usize,
    status_line: String,
    mode: Mode,
    cmd_buffer: String,
    palette_selected: usize,
    session_name: Option<String>,
    confirm_state: Option<Arc<ConfirmState>>,
    exit_armed: bool,
    // Editor
    editor: Editor,
    file_tree: Vec<editor::FileEntry>,
    file_tree_flat: Vec<(usize, editor::FileEntry)>,
    file_tree_selected: usize,
    editor_tree_focused: bool,
    editor_mouse_anchor: Option<editor::Position>,
    pending_file_changes: Vec<crate::tools::file::FileChange>,
    active_file_change: Option<crate::tools::file::FileChange>,
    reviewed_file_changes: Vec<crate::tools::file::FileChange>,
    lsp: lsp::Client,
    lsp_status: String,
    lsp_diagnostics: HashMap<PathBuf, Vec<lsp::Diagnostic>>,
    lsp_completions: Vec<lsp::CompletionItem>,
    lsp_completion_selected: usize,
    lsp_document_path: Option<PathBuf>,
    pending_code_link: Option<editor::CodeContext>,
    // Durable task runs
    task_store: TaskStore,
    task_runs: Vec<TaskRunSummary>,
    task_selected: usize,
    task_view: Option<TaskView>,
    task_scroll: usize,
    active_task: Option<ActiveTask>,
    pending_task_objective: Option<String>,
}

impl<M: CompletionModel + 'static> App<M> {
    fn new(agent: Agent<M>, provider_label: &str, provider_health: ProviderHealth) -> Self {
        let ta = chat_input(Vec::new());
        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            tab: Tab::Chat,
            messages: VecDeque::from([Message {
                kind: MsgKind::System,
                text: format!(
                    "UIntell · {provider_label}\n{}\nAlt+Enter send | /help commands | Alt+1/2/3/4/5 workspaces\n",
                    provider_health.detail()
                ),
            }]),
            input: ta,
            thinking: false,
            streaming_text: String::new(),
            active_tool: None,
            tool_args: String::new(),
            active_run: None,
            agent: Arc::new(agent),
            provider_label: provider_label.into(),
            provider_health,
            chat_history: Vec::new(),
            memory_console: GraphConsole::embedded(),
            tool_selected: 0,
            tool_output: String::new(),
            scroll: 0,
            status_line: " ready".into(),
            mode: Mode::Normal,
            cmd_buffer: String::new(),
            palette_selected: 0,
            session_name: None,
            confirm_state: None,
            exit_armed: false,
            editor: Editor::new(),
            file_tree: Vec::new(),
            file_tree_flat: Vec::new(),
            file_tree_selected: 0,
            editor_tree_focused: true,
            editor_mouse_anchor: None,
            pending_file_changes: Vec::new(),
            active_file_change: None,
            reviewed_file_changes: Vec::new(),
            lsp: lsp::Client::start(&workspace),
            lsp_status: "language server starting".into(),
            lsp_diagnostics: HashMap::new(),
            lsp_completions: Vec::new(),
            lsp_completion_selected: 0,
            lsp_document_path: None,
            pending_code_link: None,
            task_store: TaskStore::default(),
            task_runs: Vec::new(),
            task_selected: 0,
            task_view: None,
            task_scroll: 0,
            active_task: None,
            pending_task_objective: None,
        }
    }

    fn add_msg(&mut self, kind: MsgKind, text: String) {
        self.messages.push_back(Message { kind, text });
        while self.messages.len() > 1000 {
            self.messages.pop_front();
        }
    }
}

// ═══════════════════════════════════════════════════════════════
// MARKDOWN
// ═══════════════════════════════════════════════════════════════

fn preview_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

fn render_md(text: &str, base: Style) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_cb = false;
    let mut lang = String::new();
    let mut cbl: Vec<String> = Vec::new();
    for raw in text.lines() {
        let t = raw.trim();
        if let Some(stripped) = t.strip_prefix("```") {
            if in_cb {
                let s = Style::default().fg(DIM_GREEN).bg(DARK_GRAY);
                lines.push(Line::from(Span::styled(format!("  ┌─ {lang}"), s)));
                for cl in &cbl {
                    lines.push(Line::from(vec![
                        Span::styled("  │ ", s),
                        Span::styled(cl.clone(), s),
                    ]));
                }
                lines.push(Line::from(Span::styled("  └─", s)));
                cbl.clear();
                lang.clear();
                in_cb = false;
            } else {
                in_cb = true;
                lang = stripped.trim().into();
            }
            continue;
        }
        if in_cb {
            cbl.push(raw.into());
            continue;
        }
        let spans = parse_inline(t, base);
        if spans.is_empty() {
            lines.push(Line::from(Span::styled("", base)));
        } else {
            lines.push(Line::from(
                spans
                    .into_iter()
                    .map(|s| Span::styled(s.text, s.style))
                    .collect::<Vec<_>>(),
            ));
        }
    }
    if in_cb && !cbl.is_empty() {
        let s = Style::default().fg(DIM_GREEN).bg(DARK_GRAY);
        lines.push(Line::from(Span::styled(format!("  ┌─ {lang}"), s)));
        for cl in &cbl {
            lines.push(Line::from(vec![
                Span::styled("  │ ", s),
                Span::styled(cl.clone(), s),
            ]));
        }
    }
    lines
}

struct MdS {
    text: String,
    style: Style,
}

fn parse_inline(text: &str, base: Style) -> Vec<MdS> {
    if text.starts_with("# ") {
        return vec![MdS {
            text: text.into(),
            style: base.fg(CYAN).add_modifier(Modifier::BOLD),
        }];
    }
    if text.starts_with("## ") {
        return vec![MdS {
            text: text.into(),
            style: base.fg(CYAN),
        }];
    }
    if let Some(stripped) = text.strip_prefix("- ") {
        return vec![
            MdS {
                text: "  • ".into(),
                style: base.fg(GREEN),
            },
            MdS {
                text: stripped.into(),
                style: base,
            },
        ];
    }
    let mut spans = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut cur = String::new();
    while i < chars.len() {
        if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
            if !cur.is_empty() {
                spans.push(MdS {
                    text: cur.clone(),
                    style: base,
                });
                cur.clear();
            }
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '*') {
                cur.push(chars[i]);
                i += 1;
            }
            spans.push(MdS {
                text: cur.clone(),
                style: base.add_modifier(Modifier::BOLD),
            });
            cur.clear();
            i += 2;
            continue;
        }
        if chars[i] == '`' {
            if !cur.is_empty() {
                spans.push(MdS {
                    text: cur.clone(),
                    style: base,
                });
                cur.clear();
            }
            i += 1;
            while i < chars.len() && chars[i] != '`' {
                cur.push(chars[i]);
                i += 1;
            }
            spans.push(MdS {
                text: cur.clone(),
                style: Style::default().fg(YELLOW).bg(DARK_GRAY),
            });
            cur.clear();
            i += 1;
            continue;
        }
        cur.push(chars[i]);
        i += 1;
    }
    if !cur.is_empty() {
        spans.push(MdS {
            text: cur,
            style: base,
        });
    }
    spans
}

// ═══════════════════════════════════════════════════════════════
// SESSIONS
// ═══════════════════════════════════════════════════════════════

fn sessions_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
        .join(".uintell")
        .join("sessions")
}

const WORKSPACE_STATE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct WorkspaceState {
    version: u32,
    active_tab: String,
    editor_path: Option<PathBuf>,
    editor_row: usize,
    editor_col: usize,
    editor_scroll_row: usize,
    editor_tree_focused: bool,
    task_run_id: Option<String>,
    graph: GraphConsoleState,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self {
            version: WORKSPACE_STATE_VERSION,
            active_tab: "chat".into(),
            editor_path: None,
            editor_row: 0,
            editor_col: 0,
            editor_scroll_row: 0,
            editor_tree_focused: true,
            task_run_id: None,
            graph: GraphConsoleState::default(),
        }
    }
}

fn workspace_state_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
        .join(".uintell")
        .join("workspace.json")
}

fn load_workspace_state() -> std::io::Result<Option<WorkspaceState>> {
    let path = workspace_state_path();
    if !path.exists() {
        return Ok(None);
    }
    if std::fs::metadata(&path)?.len() > 1_048_576 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "workspace state exceeds 1 MiB",
        ));
    }
    let state: WorkspaceState = serde_json::from_slice(&std::fs::read(&path)?)?;
    if state.version != WORKSPACE_STATE_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsupported workspace state version {}", state.version),
        ));
    }
    Ok(Some(state))
}

fn save_workspace_state<M: CompletionModel>(app: &App<M>) -> std::io::Result<()> {
    let path = workspace_state_path();
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "workspace path has no parent",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let state = WorkspaceState {
        version: WORKSPACE_STATE_VERSION,
        active_tab: app.tab.title().to_ascii_lowercase(),
        editor_path: app.editor.file_path.clone(),
        editor_row: app.editor.cursor.row,
        editor_col: app.editor.cursor.col,
        editor_scroll_row: app.editor.cursor.scroll_row,
        editor_tree_focused: app.editor_tree_focused,
        task_run_id: app.task_view.as_ref().map(|view| view.id.clone()),
        graph: app.memory_console.state(),
    };
    let contents = serde_json::to_vec_pretty(&state)?;
    let temporary = parent.join(format!(".workspace-{}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&contents)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn restore_workspace_state<M: CompletionModel>(app: &mut App<M>, state: &WorkspaceState) {
    app.tab = match state.active_tab.as_str() {
        "memory" => Tab::Memory,
        "tools" => Tab::Tools,
        "editor" => Tab::Editor,
        "runs" => Tab::Runs,
        _ => Tab::Chat,
    };
    app.editor_tree_focused = state.editor_tree_focused;
    app.memory_console.restore_state(&state.graph);
    if let Some(id) = &state.task_run_id {
        if let Err(error) = select_task_run(app, id) {
            app.status_line = format!(" could not restore task {id}: {error}");
        }
    }
    let Some(path) = state.editor_path.as_deref().filter(|path| path.is_file()) else {
        return;
    };
    if app.editor.open(path).is_ok() {
        app.editor.cursor.row = state
            .editor_row
            .min(app.editor.buffer.len().saturating_sub(1));
        app.editor.cursor.col = state
            .editor_col
            .min(app.editor.buffer[app.editor.cursor.row].len());
        app.editor.cursor.preferred_col = app.editor.cursor.col;
        app.editor.cursor.scroll_row = state
            .editor_scroll_row
            .min(app.editor.buffer.len().saturating_sub(1));
        reveal_editor_path(app, path);
        if app.tab == Tab::Editor {
            update_editor_scroll(app);
        }
    }
}

#[derive(Serialize, Deserialize)]
struct SessionMsg {
    role: String,
    text: String,
}

fn save_session(name: &str, msgs: &[Message]) -> std::io::Result<()> {
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir)?;
    let s: Vec<SessionMsg> = msgs
        .iter()
        .map(|m| SessionMsg {
            role: match m.kind {
                MsgKind::User => "user",
                MsgKind::Agent => "agent",
                _ => "system",
            }
            .into(),
            text: m.text.clone(),
        })
        .collect();
    std::fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_string_pretty(&s)?,
    )
}

fn load_session(name: &str) -> std::io::Result<Vec<Message>> {
    let json = std::fs::read_to_string(sessions_dir().join(format!("{name}.json")))?;
    let s: Vec<SessionMsg> = serde_json::from_str(&json)?;
    Ok(s.into_iter()
        .map(|sm| Message {
            kind: match sm.role.as_str() {
                "user" => MsgKind::User,
                "agent" => MsgKind::Agent,
                _ => MsgKind::System,
            },
            text: sm.text,
        })
        .collect())
}

fn list_sessions() -> std::io::Result<Vec<String>> {
    let dir = sessions_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut n: Vec<String> = std::fs::read_dir(&dir)?
        .filter_map(|e| {
            let p = e.ok()?.path();
            if p.extension()?.to_str()? == "json" {
                p.file_stem()?.to_str().map(String::from)
            } else {
                None
            }
        })
        .collect();
    n.sort();
    Ok(n)
}

// ═══════════════════════════════════════════════════════════════
// STREAMING
// ═══════════════════════════════════════════════════════════════

fn spawn_stream<M: CompletionModel + 'static>(
    agent: Arc<Agent<M>>,
    prompt: String,
    history: Vec<RigMessage>,
    tx: mpsc::UnboundedSender<StreamEvent>,
) -> AbortHandle {
    let handle = tokio::spawn(async move {
        let mut stream = agent
            .stream_prompt(prompt)
            .history(history)
            .multi_turn(12)
            .await;
        use futures::StreamExt;
        let mut ft = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(c)) => match c {
                    StreamedAssistantContent::Text(t) => {
                        ft.push_str(&t.text);
                        let _ = tx.send(StreamEvent::TextDelta(t.text));
                    }
                    StreamedAssistantContent::ToolCall { tool_call, .. } => {
                        let _ = tx.send(StreamEvent::ToolCall {
                            name: tool_call.function.name.clone(),
                        });
                    }
                    StreamedAssistantContent::ToolCallDelta { content, .. } => match content {
                        ToolCallDeltaContent::Name(n) => {
                            let _ = tx.send(StreamEvent::ToolCall { name: n });
                        }
                        ToolCallDeltaContent::Delta(d) => {
                            let _ = tx.send(StreamEvent::ToolArgs(d));
                        }
                    },
                    _ => {}
                },
                Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                    tool_result,
                    ..
                })) => {
                    let name = tool_result.id.clone();
                    let result = tool_result
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            rig_core::message::ToolResultContent::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let _ = tx.send(StreamEvent::ToolResult {
                        name,
                        result: if result.is_empty() {
                            "(empty)".into()
                        } else {
                            result
                        },
                    });
                }
                Ok(MultiTurnStreamItem::CompletionCall(c)) => {
                    let _ = tx.send(StreamEvent::Usage {
                        input_tokens: c.usage.input_tokens,
                        output_tokens: c.usage.output_tokens,
                    });
                }
                Ok(MultiTurnStreamItem::FinalResponse(r)) => {
                    if ft.is_empty() {
                        ft = r.response().into();
                    }
                    let _ = tx.send(StreamEvent::Done(ft));
                    return;
                }
                Err(e) => {
                    let _ = tx.send(StreamEvent::Error(format!("{e}")));
                    return;
                }
                _ => {}
            }
        }
        let _ = tx.send(StreamEvent::Done(ft));
    });
    handle.abort_handle()
}

// ═══════════════════════════════════════════════════════════════
// RENDERING
// ═══════════════════════════════════════════════════════════════

fn ui<M: CompletionModel>(frame: &mut ratatui::Frame, app: &App<M>) {
    let area = frame.area();
    frame.buffer_mut().set_style(area, Style::default().bg(BG));
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    render_tabs(frame, layout[0], app);
    match app.tab {
        Tab::Chat => render_chat(frame, layout[1], app),
        Tab::Memory => app.memory_console.render(frame, layout[1]),
        Tab::Tools => render_tools(frame, layout[1], app),
        Tab::Editor => render_editor(frame, layout[1], app),
        Tab::Runs => render_task_runs(frame, layout[1], app),
    }
    render_status(frame, layout[2], app);
    if app.mode == Mode::Palette {
        render_palette(frame, app);
    }
}

fn render_tabs(frame: &mut ratatui::Frame, area: Rect, app: &App<impl CompletionModel>) {
    let titles: Vec<Line> = Tab::ALL
        .iter()
        .map(|t| {
            let s = if *t == app.tab {
                Style::default()
                    .fg(BG)
                    .bg(GREEN)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(DIM_GREEN).bg(BG)
            };
            Line::from(Span::styled(format!(" {} ", t.title()), s))
        })
        .collect();
    let hint = match app.tab {
        Tab::Chat => Span::styled(" Alt+Enter send | /help", Style::default().fg(DARK_GRAY)),
        Tab::Memory => Span::styled(
            " 1-4 graph views | Alt+1-5 agent tabs | drag/lasso | :help",
            Style::default().fg(DARK_GRAY),
        ),
        Tab::Tools => Span::styled(
            " j/k select | Enter prepare | :run tool JSON | :run! confirms",
            Style::default().fg(DARK_GRAY),
        ),
        Tab::Editor => Span::styled(
            " Tab focus | Ctrl+Space complete | Alt+C context | Alt+R run | Alt+M memory | Ctrl+P",
            Style::default().fg(DARK_GRAY),
        ),
        Tab::Runs => Span::styled(
            " j/k select | n new | r resume/retry | c cancel | Enter inspect",
            Style::default().fg(DARK_GRAY),
        ),
    };
    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(DIM_GREEN))
                .bg(BG),
        )
        .bg(BG);
    frame.render_widget(tabs, Rect { height: 2, ..area });
    frame.render_widget(
        Paragraph::new(Line::from(hint)).bg(BG),
        Rect {
            y: area.y + 2,
            height: 1,
            ..area
        },
    );
}

// ── Chat Tab ────────────────────────────────────────────────────

fn render_chat<M: CompletionModel>(frame: &mut ratatui::Frame, area: Rect, app: &App<M>) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(4)])
        .split(area);
    let msg_area = layout[0];
    let vh = msg_area.height as usize;
    let mut lines: Vec<Line> = Vec::new();

    for msg in &app.messages {
        match &msg.kind {
            MsgKind::User => lines.push(Line::from(vec![
                Span::styled("  ▶ ", Style::default().fg(CYAN)),
                Span::styled(&msg.text, Style::default().fg(CYAN)),
            ])),
            MsgKind::Agent => {
                for l in render_md(&msg.text, Style::default().fg(GREEN)) {
                    lines.push(l);
                }
            }
            MsgKind::System => {
                for l in msg.text.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("  {l}"),
                        Style::default().fg(GRAY),
                    )));
                }
            }
            MsgKind::Error => {
                for l in msg.text.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("  ✖ {l}"),
                        Style::default().fg(RED),
                    )));
                }
            }
            MsgKind::ToolCall { name } => {
                lines.push(Line::from(vec![
                    Span::styled("  ▼ ", Style::default().fg(YELLOW)),
                    Span::styled(
                        format!("🔧 {name}"),
                        Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
                    ),
                ]));
                for l in msg.text.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("    {l}"),
                        Style::default().fg(DIM_GREEN),
                    )));
                }
            }
            MsgKind::ToolResult { name } => {
                lines.push(Line::from(vec![
                    Span::styled("    ↳ ", Style::default()),
                    Span::styled(name.clone(), Style::default().fg(MAGENTA)),
                ]));
                let s = preview_text(&msg.text, 200);
                for l in s.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("      {l}"),
                        Style::default().fg(DIM_GREEN),
                    )));
                }
            }
        }
    }
    if !app.streaming_text.is_empty() {
        for l in render_md(&app.streaming_text, Style::default().fg(GREEN)) {
            lines.push(l);
        }
        lines.push(Line::from(Span::styled("  █", Style::default().fg(GREEN))));
    }
    if !app.tool_args.is_empty() {
        let s = preview_text(&app.tool_args, 100);
        lines.push(Line::from(Span::styled(
            format!("    ⚙ {s}"),
            Style::default().fg(YELLOW),
        )));
    }

    let total = lines.len();
    let so = app.scroll.min(total.saturating_sub(vh));
    let start = total.saturating_sub(vh + so);
    frame.render_widget(
        Paragraph::new(Text::from(
            lines.into_iter().skip(start).collect::<Vec<_>>(),
        ))
        .bg(BG),
        msg_area,
    );
    if total > vh {
        let mut sb = ScrollbarState::new(total.saturating_sub(vh)).position(so);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .style(Style::default().fg(DIM_GREEN).bg(BG)),
            msg_area,
            &mut sb,
        );
    }
    frame.render_widget(&app.input, layout[1]);
    if app.input.lines().join("\n").is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " Alt+Enter to send  |  /help for commands",
                Style::default().fg(DARK_GRAY),
            )))
            .bg(BG),
            layout[1],
        );
    }
}

// ── Tools Tab ───────────────────────────────────────────────────

fn render_tools<M: CompletionModel>(frame: &mut ratatui::Frame, area: Rect, app: &App<M>) {
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(area);

    let items: Vec<ListItem> = crate::tools::CATALOG
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let style = if index == app.tool_selected {
                Style::default().fg(BG).bg(GREEN)
            } else {
                Style::default().fg(GREEN).bg(BG)
            };
            ListItem::new(Line::from(Span::styled(format!(" ● {}", tool.name), style)))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.tool_selected));
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().title(" Agent Tools ").bg(BG))
            .bg(BG),
        panels[0],
        &mut state,
    );

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(3)])
        .split(panels[1]);
    if let Some(tool) = crate::tools::CATALOG.get(app.tool_selected) {
        let details = Text::from(vec![
            Line::from(Span::styled(
                format!(" {}", tool.name),
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                format!(" {}", tool.description),
                Style::default().fg(WHITE),
            )),
            Line::from(""),
            Line::from(Span::styled(" JSON", Style::default().fg(GRAY))),
            Line::from(Span::styled(
                format!(" {}", tool.example),
                Style::default().fg(YELLOW),
            )),
        ]);
        frame.render_widget(
            Paragraph::new(details)
                .block(
                    Block::default()
                        .borders(Borders::LEFT)
                        .border_style(Style::default().fg(DIM_GREEN))
                        .bg(BG),
                )
                .bg(BG)
                .wrap(Wrap { trim: false }),
            right[0],
        );
    }

    let output = if app.tool_output.is_empty() {
        "Press Enter to prepare this tool call. Use :run! for confirmation-required calls."
    } else {
        app.tool_output.as_str()
    };
    frame.render_widget(
        Paragraph::new(output)
            .block(
                Block::default()
                    .borders(Borders::LEFT | Borders::TOP)
                    .title(" Output ")
                    .border_style(Style::default().fg(DIM_GREEN))
                    .bg(BG),
            )
            .bg(BG)
            .wrap(Wrap { trim: false }),
        right[1],
    );
}

// ── Durable Runs Tab ───────────────────────────────────────────

fn render_task_runs<M: CompletionModel>(frame: &mut ratatui::Frame, area: Rect, app: &App<M>) {
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(32), Constraint::Percentage(68)])
        .split(area);
    let items = app
        .task_runs
        .iter()
        .enumerate()
        .map(|(index, run)| {
            let selected = index == app.task_selected;
            let base = task_status_style(run.status);
            let style = if selected {
                Style::default().fg(BG).bg(GREEN)
            } else {
                base.bg(BG)
            };
            ListItem::new(vec![
                Line::from(Span::styled(
                    format!(
                        " {}  {}",
                        run.status.label(),
                        preview_text(&run.objective, 28)
                    ),
                    style,
                )),
                Line::from(Span::styled(
                    format!(
                        "    {}/{}  {}  {}",
                        run.current_step.min(run.total_steps),
                        run.total_steps,
                        age_label(run.updated_at),
                        preview_text(&run.id, 18)
                    ),
                    if selected {
                        style
                    } else {
                        Style::default().fg(GRAY).bg(BG)
                    },
                )),
            ])
        })
        .collect::<Vec<_>>();
    let mut list_state = ListState::default();
    if !app.task_runs.is_empty() {
        list_state.select(Some(app.task_selected));
    }
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().title(" Durable Runs ").bg(BG))
            .highlight_symbol(""),
        panels[0],
        &mut list_state,
    );

    let Some(view) = &app.task_view else {
        let text = Text::from(vec![
            Line::from(Span::styled(
                "No task run selected",
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Press n or use :new [--remember] <objective> to start a durable coding run.",
                Style::default().fg(WHITE),
            )),
            Line::from(""),
            Line::from(Span::styled(
                format!("Checkpoints: {}", app.task_store.root().display()),
                Style::default().fg(GRAY),
            )),
        ]);
        frame.render_widget(
            Paragraph::new(text)
                .block(
                    Block::default()
                        .borders(Borders::LEFT)
                        .border_style(Style::default().fg(DIM_GREEN))
                        .bg(BG),
                )
                .bg(BG)
                .wrap(Wrap { trim: false }),
            panels[1],
        );
        return;
    };

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Percentage(38),
            Constraint::Min(5),
        ])
        .split(panels[1]);
    let header = Text::from(vec![
        Line::from(vec![
            Span::styled(
                format!(" {} ", view.status.label()),
                task_status_style(view.status).add_modifier(Modifier::BOLD),
            ),
            Span::styled(&view.id, Style::default().fg(GRAY)),
        ]),
        Line::from(Span::styled(
            format!(" {}", view.objective),
            Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!(" {}", view.workspace.display()),
            Style::default().fg(DIM_GREEN),
        )),
        Line::from(vec![
            Span::styled(
                format!(" provider {}", view.provider),
                Style::default().fg(CYAN),
            ),
            Span::styled(
                if view.memory_writes {
                    "  memory writable"
                } else {
                    "  memory read-only"
                },
                Style::default().fg(if view.memory_writes { YELLOW } else { GRAY }),
            ),
            Span::styled(
                format!(
                    "  step {}/{}  repairs {}",
                    view.current_step.min(view.steps.len()),
                    view.steps.len(),
                    view.repair_rounds
                ),
                Style::default().fg(YELLOW),
            ),
        ]),
        Line::from(Span::styled(
            format!(
                " created {}  updated {}{}",
                age_label(view.created_at),
                age_label(view.updated_at),
                view.finished_at
                    .map(|finished| format!("  finished {}", age_label(finished)))
                    .unwrap_or_default()
            ),
            Style::default().fg(GRAY),
        )),
    ]);
    frame.render_widget(
        Paragraph::new(header)
            .block(
                Block::default()
                    .borders(Borders::LEFT)
                    .border_style(Style::default().fg(DIM_GREEN))
                    .bg(BG),
            )
            .bg(BG)
            .wrap(Wrap { trim: false }),
        right[0],
    );

    let step_lines = view
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let marker = if index == view.current_step && view.status == TaskRunStatus::Running {
                ">"
            } else {
                " "
            };
            Line::from(vec![
                Span::styled(
                    format!(" {marker} {:>2}. {:<9} ", index + 1, step.status.label()),
                    task_step_style(step.status),
                ),
                Span::styled(&step.title, Style::default().fg(WHITE)),
                Span::styled(
                    format!("  attempt {}", step.attempt),
                    Style::default().fg(GRAY),
                ),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(step_lines)
            .block(
                Block::default()
                    .borders(Borders::LEFT | Borders::TOP)
                    .title(" Quality Gates ")
                    .border_style(Style::default().fg(DIM_GREEN))
                    .bg(BG),
            )
            .bg(BG),
        right[1],
    );

    let mut event_lines = Vec::new();
    for event in &view.events {
        event_lines.push(Line::from(vec![
            Span::styled(
                format!(" {:<8} ", event.kind.label()),
                task_event_style(event.kind),
            ),
            Span::styled(&event.title, Style::default().fg(WHITE)),
        ]));
        if !event.detail.is_empty() {
            event_lines.push(Line::from(Span::styled(
                format!(
                    "           {}",
                    preview_text(&event.detail.replace('\n', " "), 180)
                ),
                Style::default().fg(GRAY),
            )));
        }
    }
    if let Some(error) = &view.error {
        event_lines.push(Line::from(Span::styled(
            format!(" error     {error}"),
            Style::default().fg(RED),
        )));
    }
    if let Some(result) = &view.result {
        event_lines.push(Line::from(""));
        event_lines.push(Line::from(Span::styled(
            " FINAL REPORT",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )));
        event_lines.extend(render_md(result, Style::default().fg(GREEN)));
    }
    let visible = right[2].height.saturating_sub(2) as usize;
    let max_scroll = event_lines.len().saturating_sub(visible);
    let scroll = app.task_scroll.min(max_scroll);
    let start = max_scroll.saturating_sub(scroll);
    frame.render_widget(
        Paragraph::new(event_lines.into_iter().skip(start).collect::<Vec<_>>())
            .block(
                Block::default()
                    .borders(Borders::LEFT | Borders::TOP)
                    .title(" Execution Timeline ")
                    .border_style(Style::default().fg(DIM_GREEN))
                    .bg(BG),
            )
            .bg(BG)
            .wrap(Wrap { trim: false }),
        right[2],
    );
}

fn task_status_style(status: TaskRunStatus) -> Style {
    match status {
        TaskRunStatus::Completed => Style::default().fg(GREEN),
        TaskRunStatus::Running => Style::default().fg(YELLOW),
        TaskRunStatus::NeedsAttention | TaskRunStatus::Failed => Style::default().fg(RED),
        TaskRunStatus::Cancelled | TaskRunStatus::Paused => Style::default().fg(MAGENTA),
        TaskRunStatus::Pending => Style::default().fg(GRAY),
    }
}

fn task_step_style(status: crate::task_run::TaskStepStatus) -> Style {
    match status {
        crate::task_run::TaskStepStatus::Completed => Style::default().fg(GREEN),
        crate::task_run::TaskStepStatus::Running => Style::default().fg(YELLOW),
        crate::task_run::TaskStepStatus::Failed => Style::default().fg(RED),
        crate::task_run::TaskStepStatus::Cancelled => Style::default().fg(MAGENTA),
        crate::task_run::TaskStepStatus::Pending => Style::default().fg(GRAY),
    }
}

fn task_event_style(kind: crate::task_run::TaskEventKind) -> Style {
    use crate::task_run::TaskEventKind;
    match kind {
        TaskEventKind::Completed | TaskEventKind::StepCompleted => Style::default().fg(GREEN),
        TaskEventKind::Failed => Style::default().fg(RED),
        TaskEventKind::Warning | TaskEventKind::RepairScheduled => Style::default().fg(YELLOW),
        TaskEventKind::Cancelled => Style::default().fg(MAGENTA),
        TaskEventKind::ToolCall | TaskEventKind::ToolResult => Style::default().fg(CYAN),
        _ => Style::default().fg(GRAY),
    }
}

fn age_label(timestamp_ms: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let seconds = now.saturating_sub(timestamp_ms) / 1_000;
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

fn palette_items(query: &str) -> Vec<PaletteItem> {
    let mut items = vec![
        PaletteItem {
            title: "Go to Chat".into(),
            detail: "Conversation and streaming agent".into(),
            action: PaletteAction::SwitchTab(Tab::Chat),
        },
        PaletteItem {
            title: "Go to Memory".into(),
            detail: "Knowledge graph operations console".into(),
            action: PaletteAction::SwitchTab(Tab::Memory),
        },
        PaletteItem {
            title: "Go to Tools".into(),
            detail: "Inspect and run registered tools".into(),
            action: PaletteAction::SwitchTab(Tab::Tools),
        },
        PaletteItem {
            title: "Go to Editor".into(),
            detail: "Workspace code editor".into(),
            action: PaletteAction::SwitchTab(Tab::Editor),
        },
        PaletteItem {
            title: "Go to Runs".into(),
            detail: "Durable autonomous tasks".into(),
            action: PaletteAction::SwitchTab(Tab::Runs),
        },
        PaletteItem {
            title: "Memory: Graph".into(),
            detail: "Visual topology canvas".into(),
            action: PaletteAction::MemoryView("graph"),
        },
        PaletteItem {
            title: "Memory: Explorer".into(),
            detail: "Facts, relations, and details".into(),
            action: PaletteAction::MemoryView("explorer"),
        },
        PaletteItem {
            title: "Memory: Query".into(),
            detail: "Multiline SurrealQL workbench".into(),
            action: PaletteAction::MemoryView("query"),
        },
        PaletteItem {
            title: "Memory: Analytics".into(),
            detail: "Graph health and distribution".into(),
            action: PaletteAction::MemoryView("analytics"),
        },
        PaletteItem {
            title: "Memory: Refresh".into(),
            detail: "Reload the SurrealDB snapshot".into(),
            action: PaletteAction::RefreshMemory,
        },
        PaletteItem {
            title: "Editor: Browse files".into(),
            detail: "Load and focus the workspace tree".into(),
            action: PaletteAction::BrowseEditor,
        },
        PaletteItem {
            title: "Editor: Save file".into(),
            detail: "Write the current buffer".into(),
            action: PaletteAction::SaveEditor,
        },
        PaletteItem {
            title: "Editor: Add context to Chat".into(),
            detail: "Stage the selection or current file in Chat".into(),
            action: PaletteAction::EditorToChat,
        },
        PaletteItem {
            title: "Editor: Start run with context".into(),
            detail: "Stage a durable task using the selection or file".into(),
            action: PaletteAction::EditorToRun,
        },
        PaletteItem {
            title: "Editor: Pin context to Memory".into(),
            detail: "Create a navigable code-location knowledge unit".into(),
            action: PaletteAction::EditorPinMemory,
        },
        PaletteItem {
            title: "Editor: Go to definition".into(),
            detail: "Ask the language server for the current symbol".into(),
            action: PaletteAction::EditorDefinition,
        },
        PaletteItem {
            title: "Review: Accept all hunks".into(),
            detail: "Apply every pending agent edit in this file".into(),
            action: PaletteAction::ReviewAcceptAll,
        },
        PaletteItem {
            title: "Review: Reject all hunks".into(),
            detail: "Restore the file before the agent edit".into(),
            action: PaletteAction::ReviewRejectAll,
        },
        PaletteItem {
            title: "Review: Undo accepted change".into(),
            detail: "Restore the file before its last accepted review".into(),
            action: PaletteAction::ReviewUndo,
        },
        PaletteItem {
            title: "Runs: New task".into(),
            detail: "Create a durable coding run".into(),
            action: PaletteAction::NewTask,
        },
        PaletteItem {
            title: "Runs: Resume task".into(),
            detail: "Resume or retry the selected run".into(),
            action: PaletteAction::ResumeTask,
        },
        PaletteItem {
            title: "Runs: Cancel task".into(),
            detail: "Cancel the active run and keep its checkpoint".into(),
            action: PaletteAction::CancelTask,
        },
    ];
    items.extend(
        crate::tools::CATALOG
            .iter()
            .enumerate()
            .map(|(index, tool)| PaletteItem {
                title: format!("Tool: {}", tool.name),
                detail: tool.description.into(),
                action: PaletteAction::SelectTool(index),
            }),
    );

    let terms = query
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    if terms.is_empty() {
        return items;
    }
    items
        .into_iter()
        .filter(|item| {
            let haystack = format!("{} {}", item.title, item.detail).to_ascii_lowercase();
            terms.iter().all(|term| haystack.contains(term))
        })
        .collect()
}

fn render_palette<M: CompletionModel>(frame: &mut ratatui::Frame, app: &App<M>) {
    let area = frame.area();
    let width = 84.min(area.width.saturating_sub(2));
    let height = 22.min(area.height.saturating_sub(2));
    if width < 24 || height < 6 {
        return;
    }
    let dialog = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, dialog);
    frame.render_widget(
        Block::default()
            .title(" Command Palette · Ctrl+P ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(GREEN))
            .bg(BG),
        dialog,
    );
    let inner = dialog.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(2),
            Constraint::Length(1),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" > ", Style::default().fg(CYAN)),
            Span::styled(app.cmd_buffer.clone(), Style::default().fg(WHITE)),
            Span::styled("█", Style::default().fg(GREEN)),
        ]))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(DIM_GREEN)),
        )
        .bg(BG),
        parts[0],
    );

    let filtered = palette_items(&app.cmd_buffer);
    let selected = app.palette_selected.min(filtered.len().saturating_sub(1));
    if filtered.is_empty() {
        frame.render_widget(
            Paragraph::new(" No matching command").fg(GRAY).bg(BG),
            parts[1],
        );
    } else {
        let rows = filtered
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let style = if index == selected {
                    Style::default().fg(BG).bg(GREEN)
                } else {
                    Style::default().fg(WHITE).bg(BG)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {:<27}", item.title), style),
                    Span::styled(preview_text(&item.detail, 42), style),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default();
        state.select(Some(selected));
        frame.render_stateful_widget(List::new(rows).bg(BG), parts[1], &mut state);
    }
    frame.render_widget(
        Paragraph::new(format!(" {} command(s)", filtered.len()))
            .fg(DARK_GRAY)
            .bg(BG),
        parts[2],
    );
}

fn execute_palette_action<M: CompletionModel + 'static>(
    app: &mut App<M>,
    action: PaletteAction,
    task_updates: mpsc::UnboundedSender<TaskNotification>,
) {
    app.mode = Mode::Normal;
    app.cmd_buffer.clear();
    match action {
        PaletteAction::SwitchTab(tab) => {
            app.tab = tab;
            if tab == Tab::Editor {
                update_editor_scroll(app);
            }
            if tab == Tab::Runs {
                if let Err(error) = load_task_runs(app) {
                    app.status_line = format!(" task refresh failed: {error}");
                }
            }
        }
        PaletteAction::MemoryView(view) => {
            app.tab = Tab::Memory;
            app.memory_console.open_view(view);
        }
        PaletteAction::RefreshMemory => {
            app.tab = Tab::Memory;
            app.memory_console.request_refresh();
            app.status_line = " refreshing graph memory".into();
        }
        PaletteAction::BrowseEditor => {
            app.tab = Tab::Editor;
            load_editor_tree(app);
            app.editor_tree_focused = true;
            update_editor_scroll(app);
            app.editor.status_msg = "workspace tree focused".into();
        }
        PaletteAction::SaveEditor => {
            app.tab = Tab::Editor;
            if let Err(error) = save_editor_buffer(app) {
                app.editor.status_msg = format!("save error: {error}");
            }
        }
        PaletteAction::EditorToChat => stage_editor_context(app, false),
        PaletteAction::EditorToRun => stage_editor_context(app, true),
        PaletteAction::EditorPinMemory => queue_editor_memory_link(app),
        PaletteAction::EditorDefinition => request_editor_definition(app),
        PaletteAction::ReviewAcceptAll => {
            app.tab = Tab::Editor;
            persist_review_decision(app, editor::ReviewDecision::Accepted, true);
        }
        PaletteAction::ReviewRejectAll => {
            app.tab = Tab::Editor;
            persist_review_decision(app, editor::ReviewDecision::Rejected, true);
        }
        PaletteAction::ReviewUndo => {
            app.tab = Tab::Editor;
            undo_reviewed_file_change(app);
        }
        PaletteAction::NewTask => {
            app.tab = Tab::Runs;
            app.mode = Mode::Command;
            app.cmd_buffer = "new ".into();
        }
        PaletteAction::ResumeTask => {
            app.tab = Tab::Runs;
            if let Err(error) = resume_selected_task(app, task_updates) {
                app.status_line = format!(" task resume failed: {error}");
            }
        }
        PaletteAction::CancelTask => {
            app.tab = Tab::Runs;
            if !cancel_task(app) {
                app.status_line = " no durable task is active".into();
            }
        }
        PaletteAction::SelectTool(index) => {
            app.tab = Tab::Tools;
            app.tool_selected = index.min(crate::tools::CATALOG.len().saturating_sub(1));
            if let Some(tool) = crate::tools::CATALOG.get(app.tool_selected) {
                app.status_line = format!(" selected tool {}", tool.name);
            }
        }
    }
}

// ── Status Bar ──────────────────────────────────────────────────

fn render_status<M: CompletionModel>(frame: &mut ratatui::Frame, area: Rect, app: &App<M>) {
    let mode_str = match app.mode {
        Mode::Normal => "NORMAL",
        Mode::Command => "CMD",
        Mode::Search => "SEARCH",
        Mode::Palette => "PALETTE",
    };
    let (loaded_facts, total_facts) = app.memory_console.node_counts();
    let db_info = if total_facts > 0 {
        format!(" · {loaded_facts}/{total_facts} facts")
    } else {
        String::new()
    };
    let activity = if app.thinking {
        " · chat running".to_string()
    } else if let Some(task) = &app.active_task {
        format!(" · run {}", preview_text(&task.id, 18))
    } else {
        String::new()
    };
    let text = format!(
        " {} · {mode_str} · {}{db_info}{activity}{}",
        app.tab.title(),
        app.provider_health.badge(),
        if app.thinking { "" } else { &app.status_line }
    );
    let style = if app.thinking || app.active_task.is_some() {
        Style::default().fg(BG).bg(GREEN)
    } else {
        Style::default().fg(BG).bg(DIM_GREEN)
    };
    frame.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), area);
    // Command bar overlay
    if matches!(app.mode, Mode::Command | Mode::Search) {
        let (pfx, txt) = match app.mode {
            Mode::Command => (":", app.cmd_buffer.as_str()),
            Mode::Search => ("/", app.cmd_buffer.as_str()),
            _ => ("", app.cmd_buffer.as_str()),
        };
        let line = if txt.is_empty() {
            Line::from(Span::styled(
                format!("{pfx}█"),
                Style::default().fg(GREEN).bg(BG),
            ))
        } else {
            Line::from(vec![
                Span::styled(pfx, Style::default().fg(YELLOW).bg(BG)),
                Span::styled(txt.to_string(), Style::default().fg(WHITE).bg(BG)),
                Span::styled("█", Style::default().fg(GREEN).bg(BG)),
            ])
        };
        frame.render_widget(
            Paragraph::new(line).bg(BG),
            Rect {
                y: area.y.saturating_sub(1),
                height: 1,
                ..area
            },
        );
    }
}

// ═══════════════════════════════════════════════════════════════
// CHAT COMMANDS
// ═══════════════════════════════════════════════════════════════

fn handle_chat_cmd<M: CompletionModel>(app: &mut App<M>, cmd: &str) -> bool {
    let parts: Vec<&str> = cmd.splitn(2, ' ').collect();
    match parts[0] {
        "/exit" | "/quit" => {
            if app.active_file_change.is_some() {
                app.tab = Tab::Editor;
                app.editor.status_msg = "resolve the agent change review before exiting".into();
                app.status_line = " exit blocked by pending change review".into();
            } else if app.editor.dirty {
                app.tab = Tab::Editor;
                app.editor.status_msg = "unsaved changes; use :w or :discard before exiting".into();
                app.status_line = " exit blocked by unsaved editor changes".into();
            } else {
                return true;
            }
        }
        "/clear" => {
            app.messages.clear();
            app.chat_history.clear();
            app.scroll = 0;
        }
        "/help" => {
            app.add_msg(MsgKind::System, "Commands: /task /runs /health /exit /clear /save /load /sessions\nKeys: Alt+1=Chat Alt+2=Memory Alt+3=Tools Alt+4=Editor Alt+5=Runs Alt+Enter=send".into());
        }
        "/task" => {
            let objective = parts.get(1).copied().unwrap_or("").trim();
            if objective.is_empty() {
                app.add_msg(MsgKind::Error, "Usage: /task <objective>".into());
            } else {
                app.pending_task_objective = Some(objective.to_string());
                app.tab = Tab::Runs;
                app.status_line = " preparing durable task run".into();
            }
        }
        "/runs" => {
            app.tab = Tab::Runs;
            if let Err(error) = load_task_runs(app) {
                app.status_line = format!(" could not load durable runs: {error}");
            }
        }
        "/health" => app.add_msg(
            if app.provider_health.is_ready() {
                MsgKind::System
            } else {
                MsgKind::Error
            },
            app.provider_health.detail().to_string(),
        ),
        "/save" => {
            let n = parts.get(1).copied().unwrap_or("default");
            match save_session(n, &app.messages.iter().cloned().collect::<Vec<_>>()) {
                Ok(()) => {
                    app.session_name = Some(n.into());
                    app.add_msg(MsgKind::System, format!("Saved '{n}'"));
                }
                Err(e) => app.add_msg(MsgKind::Error, format!("Save: {e}")),
            }
        }
        "/load" => {
            let n = parts.get(1).copied().unwrap_or("default");
            match load_session(n) {
                Ok(m) => {
                    app.messages = m.into();
                    app.chat_history = app
                        .messages
                        .iter()
                        .filter_map(|message| match message.kind {
                            MsgKind::User => Some(RigMessage::user(message.text.clone())),
                            MsgKind::Agent => Some(RigMessage::assistant(message.text.clone())),
                            _ => None,
                        })
                        .collect();
                    app.session_name = Some(n.into());
                    app.add_msg(MsgKind::System, format!("Loaded '{n}'"));
                }
                Err(e) => app.add_msg(MsgKind::Error, format!("Load: {e}")),
            }
        }
        "/sessions" => match list_sessions() {
            Ok(n) => app.add_msg(
                MsgKind::System,
                if n.is_empty() {
                    "No sessions".into()
                } else {
                    format!("Sessions:\n  {}", n.join("\n  "))
                },
            ),
            Err(e) => app.add_msg(MsgKind::Error, format!("{e}")),
        },
        _ => app.add_msg(MsgKind::System, format!("Unknown: {cmd}")),
    }
    false
}

// ═══════════════════════════════════════════════════════════════
// EDITOR RENDERING
// ═══════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
struct EditorLayout {
    tree: Rect,
    code: Rect,
    status: Rect,
    inspector: Option<Rect>,
}

fn editor_layout(area: Rect) -> EditorLayout {
    let wide = area.width >= 120;
    let constraints = if wide {
        vec![
            Constraint::Percentage(18),
            Constraint::Percentage(62),
            Constraint::Percentage(20),
        ]
    } else {
        vec![Constraint::Percentage(20), Constraint::Percentage(80)]
    };
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(panels[1]);
    EditorLayout {
        tree: panels[0],
        code: right[0],
        status: right[1],
        inspector: if wide { panels.get(2).copied() } else { None },
    }
}

fn editor_tree_offset<M: CompletionModel>(app: &App<M>, height: usize) -> usize {
    if height == 0 || app.file_tree_flat.len() <= height {
        return 0;
    }
    app.file_tree_selected
        .saturating_sub(height / 2)
        .min(app.file_tree_flat.len() - height)
}

fn editor_char_boundary(line: &str, mut col: usize) -> usize {
    col = col.min(line.len());
    while col > 0 && !line.is_char_boundary(col) {
        col -= 1;
    }
    col
}

fn render_editor<M: CompletionModel>(frame: &mut ratatui::Frame, area: Rect, app: &App<M>) {
    let layout = editor_layout(area);
    let diagnostics = app
        .editor
        .file_path
        .as_deref()
        .map(normalize_path)
        .and_then(|path| app.lsp_diagnostics.get(&path));

    // Left: file tree
    if app.file_tree_flat.is_empty() && app.editor.file_path.is_none() {
        // Load file tree on first render
        // Can't mutate app in render, so we defer to run_loop
        let hint =
            Paragraph::new(":e <file> to open\n:e . to load tree\n\nj/k navigate\nEnter open")
                .block(Block::default().borders(Borders::NONE).bg(BG))
                .bg(BG);
        frame.render_widget(hint, layout.tree);
    } else if !app.file_tree_flat.is_empty() {
        let tree_height = layout.tree.height as usize;
        let tree_offset = editor_tree_offset(app, tree_height);
        let items: Vec<ListItem> = app
            .file_tree_flat
            .iter()
            .enumerate()
            .skip(tree_offset)
            .take(tree_height)
            .map(|(i, (depth, entry))| {
                let prefix = "  ".repeat(*depth);
                let icon = if entry.is_dir { "📁" } else { "📄" };
                let style = if i == app.file_tree_selected && app.editor_tree_focused {
                    Style::default().fg(BG).bg(GREEN)
                } else if entry.is_dir {
                    Style::default().fg(CYAN).bg(BG)
                } else {
                    Style::default().fg(GREEN).bg(BG)
                };
                ListItem::new(Line::from(Span::styled(
                    format!("{prefix}{icon} {}", entry.name),
                    style,
                )))
            })
            .collect();
        let mut st = ListState::default();
        st.select(Some(app.file_tree_selected.saturating_sub(tree_offset)));
        frame.render_stateful_widget(
            List::new(items)
                .block(Block::default().borders(Borders::NONE).bg(BG))
                .bg(BG),
            layout.tree,
            &mut st,
        );
    }

    // Code with line numbers
    let mut lines: Vec<Line> = Vec::new();
    let visible_h = layout.code.height as usize;
    let scroll = app.editor.cursor.scroll_row;

    if let Some(review_lines) = app.editor.review_display_lines() {
        let review = app.editor.change_review.as_ref().expect("review exists");
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    " Change Review · hunk {}/{} · {}/{} resolved ",
                    review
                        .selected_hunk
                        .saturating_add(1)
                        .min(review.hunk_count()),
                    review.hunk_count(),
                    review.resolved_count(),
                    review.hunk_count()
                ),
                Style::default()
                    .fg(BG)
                    .bg(CYAN)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  [/] navigate  a/r decide  A/R all",
                Style::default().fg(GRAY),
            ),
        ]));
        let selected_line = review_lines
            .iter()
            .position(|line| line.selected)
            .unwrap_or(0);
        let capacity = visible_h.saturating_sub(1).max(1);
        let offset = selected_line
            .saturating_sub(capacity / 2)
            .min(review_lines.len().saturating_sub(capacity));
        for display in review_lines.into_iter().skip(offset).take(capacity) {
            let base = match display.decision {
                editor::ReviewDecision::Pending => Style::default(),
                editor::ReviewDecision::Accepted => {
                    Style::default().bg(Color::Rgb(0x08, 0x24, 0x08))
                }
                editor::ReviewDecision::Rejected => {
                    Style::default().bg(Color::Rgb(0x2A, 0x08, 0x08))
                }
            };
            let hunk_marker = match (display.hunk, display.selected, display.decision) {
                (None, _, _) => "  ",
                (Some(_), true, _) => "▸ ",
                (Some(_), false, editor::ReviewDecision::Accepted) => "✓ ",
                (Some(_), false, editor::ReviewDecision::Rejected) => "× ",
                (Some(_), false, editor::ReviewDecision::Pending) => "· ",
            };
            let (change_marker, text, color) = match display.line {
                editor::DiffLine::Unchanged(text) => (" ", text, GRAY),
                editor::DiffLine::Added(text) => ("+", text, GREEN),
                editor::DiffLine::Removed(text) => ("-", text, RED),
            };
            lines.push(Line::from(vec![
                Span::styled(
                    hunk_marker,
                    base.fg(if display.selected { YELLOW } else { GRAY }),
                ),
                Span::styled(change_marker, base.fg(color).add_modifier(Modifier::BOLD)),
                Span::styled(format!(" {text}"), base.fg(color)),
            ]));
        }
    } else if app.editor.show_diff && !app.editor.diff_lines.is_empty() {
        // Show diff view
        lines.push(Line::from(Span::styled(
            " ── Diff View ──",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )));
        for dline in &app.editor.diff_lines {
            match dline {
                editor::DiffLine::Unchanged(s) => {
                    lines.push(Line::from(vec![
                        Span::styled("  ", Style::default()),
                        Span::styled(s.clone(), Style::default().fg(GRAY)),
                    ]));
                }
                editor::DiffLine::Added(s) => {
                    lines.push(Line::from(vec![
                        Span::styled(
                            " +",
                            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(s.clone(), Style::default().fg(GREEN)),
                    ]));
                }
                editor::DiffLine::Removed(s) => {
                    lines.push(Line::from(vec![
                        Span::styled(" -", Style::default().fg(RED).add_modifier(Modifier::BOLD)),
                        Span::styled(s.clone(), Style::default().fg(RED)),
                    ]));
                }
            }
        }
    } else {
        for (ln, (i, line)) in app
            .editor
            .buffer
            .iter()
            .enumerate()
            .skip(scroll)
            .enumerate()
        {
            if ln >= visible_h {
                break;
            }
            let num = i + 1;
            let is_cursor = i == app.editor.cursor.row;
            let num_style = if is_cursor {
                Style::default().fg(BG).bg(GREEN)
            } else {
                Style::default().fg(DARK_GRAY).bg(BG)
            };
            let text_style = if is_cursor {
                Style::default().fg(WHITE).bg(DARK_GRAY)
            } else {
                Style::default().fg(GREEN).bg(BG)
            };
            let diagnostic_severity = diagnostics.and_then(|items| {
                items
                    .iter()
                    .filter(|diagnostic| diagnostic.range.start.line == i)
                    .map(|diagnostic| diagnostic.severity)
                    .min()
            });
            let cursor_marker = match diagnostic_severity {
                Some(1) => "E",
                Some(2) => "W",
                Some(_) => "I",
                None if is_cursor && app.editor.mode == editor::Mode::Normal => "▶",
                None => " ",
            };

            let visible_start = editor_char_boundary(line, app.editor.cursor.scroll_col);
            let mut spans = vec![Span::styled(format!("{cursor_marker}{num:>4} "), num_style)];
            let selection =
                app.editor
                    .selection_byte_range(i)
                    .filter(|(selection_start, selection_end)| {
                        *selection_end > visible_start
                            || (*selection_start == *selection_end && *selection_end == line.len())
                    });
            if let Some((selection_start, selection_end)) = selection {
                let selection_start = selection_start.max(visible_start).min(line.len());
                let selection_end = selection_end.max(visible_start).min(line.len());
                spans.push(Span::styled(
                    line[visible_start..selection_start].to_string(),
                    text_style,
                ));
                let selected = &line[selection_start..selection_end];
                spans.push(Span::styled(
                    if selected.is_empty() {
                        " ".to_string()
                    } else {
                        selected.to_string()
                    },
                    Style::default().fg(BG).bg(CYAN),
                ));
                spans.push(Span::styled(line[selection_end..].to_string(), text_style));
            } else if !app.editor.search_term.is_empty() {
                let visible = &line[visible_start..];
                if let Some(position) = visible.find(&app.editor.search_term) {
                    let match_end = position + app.editor.search_term.len();
                    spans.push(Span::styled(visible[..position].to_string(), text_style));
                    spans.push(Span::styled(
                        visible[position..match_end].to_string(),
                        Style::default().fg(BG).bg(YELLOW),
                    ));
                    spans.push(Span::styled(visible[match_end..].to_string(), text_style));
                } else {
                    spans.push(Span::styled(visible.to_string(), text_style));
                }
            } else {
                spans.push(Span::styled(line[visible_start..].to_string(), text_style));
            }
            lines.push(Line::from(spans));
        }
    }

    frame.render_widget(Paragraph::new(Text::from(lines)).bg(BG), layout.code);

    if !app.editor_tree_focused
        && !app.editor.show_diff
        && app.editor.cursor.row >= scroll
        && app.editor.cursor.row < scroll + visible_h
    {
        let line = &app.editor.buffer[app.editor.cursor.row];
        let visible_start = editor_char_boundary(line, app.editor.cursor.scroll_col);
        let cursor_col = editor_char_boundary(line, app.editor.cursor.col);
        if cursor_col >= visible_start {
            let display_col = UnicodeWidthStr::width(&line[visible_start..cursor_col]);
            let x = layout
                .code
                .x
                .saturating_add(6)
                .saturating_add(u16::try_from(display_col).unwrap_or(u16::MAX));
            let y = layout
                .code
                .y
                .saturating_add((app.editor.cursor.row - scroll) as u16);
            if x < layout.code.right() && y < layout.code.bottom() {
                frame.set_cursor_position((x, y));
            }
        }
    }

    // Status bar
    let mode_str = if app.editor.change_review.is_some() {
        "REVIEW"
    } else if app.editor_tree_focused {
        "TREE"
    } else {
        match app.editor.mode {
            editor::Mode::Normal => "NORMAL",
            editor::Mode::Insert => "INSERT",
            editor::Mode::Command => "COMMAND",
            editor::Mode::Visual => "VISUAL",
            editor::Mode::VisualLine => "V-LINE",
        }
    };
    let filename = app
        .editor
        .file_path
        .as_ref()
        .map(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        })
        .unwrap_or_else(|| "[No Name]".into());
    let dirty = if app.editor.dirty { " [+]" } else { "" };
    let character_col = app.editor.buffer[app.editor.cursor.row][..app.editor.cursor.col]
        .chars()
        .count()
        + 1;
    let pos = format!("{}:{character_col}", app.editor.cursor.row + 1);
    let cmdline = if app.editor.mode == editor::Mode::Command {
        format!(":{}", app.editor.cmd_buffer)
    } else if !app.editor.search_term.is_empty() {
        format!("/{}", app.editor.search_term)
    } else {
        app.editor.status_msg.clone()
    };

    let st_line = Line::from(vec![
        Span::styled(format!(" {mode_str} "), Style::default().fg(BG).bg(CYAN)),
        Span::styled(
            format!(" {filename}{dirty} "),
            Style::default().fg(GREEN).bg(BG),
        ),
        Span::styled(format!(" {pos} "), Style::default().fg(GRAY).bg(BG)),
        Span::styled(format!(" {cmdline}"), Style::default().fg(WHITE).bg(BG)),
    ]);

    frame.render_widget(Paragraph::new(st_line).bg(BG), layout.status);

    if let Some(inspector) = layout.inspector {
        render_editor_inspector(frame, inspector, app);
    }
    render_completion_menu(frame, layout, app);
}

fn render_editor_inspector<M: CompletionModel>(
    frame: &mut ratatui::Frame,
    area: Rect,
    app: &App<M>,
) {
    let diagnostics = app
        .editor
        .file_path
        .as_deref()
        .map(normalize_path)
        .and_then(|path| app.lsp_diagnostics.get(&path));
    let error_count = diagnostics.map_or(0, |items| {
        items
            .iter()
            .filter(|diagnostic| diagnostic.severity == 1)
            .count()
    });
    let warning_count = diagnostics.map_or(0, |items| {
        items
            .iter()
            .filter(|diagnostic| diagnostic.severity == 2)
            .count()
    });
    let mut lines = vec![
        Line::from(Span::styled(
            " CODE INTELLIGENCE",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!(" {}", preview_text(&app.lsp_status, 36)),
            Style::default().fg(if app.lsp.is_ready() { GREEN } else { GRAY }),
        )),
        Line::from(vec![
            Span::styled(format!(" {error_count} errors"), Style::default().fg(RED)),
            Span::styled(
                format!("  {warning_count} warnings"),
                Style::default().fg(YELLOW),
            ),
        ]),
        Line::from(""),
    ];
    if let Some(diagnostics) = diagnostics {
        for diagnostic in diagnostics.iter().take(8) {
            let color = match diagnostic.severity {
                1 => RED,
                2 => YELLOW,
                3 => CYAN,
                _ => GRAY,
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {:>4} ", diagnostic.range.start.line + 1),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    preview_text(&diagnostic.message.replace('\n', " "), 30),
                    Style::default().fg(WHITE),
                ),
            ]));
        }
    }

    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            " AGENT ACTIVITY",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )),
    ]);
    if let Some(tool) = &app.active_tool {
        lines.push(Line::from(vec![
            Span::styled(" running ", Style::default().fg(BG).bg(YELLOW)),
            Span::styled(tool, Style::default().fg(YELLOW)),
        ]));
    }
    if let Some(task) = &app.task_view {
        for event in task.events.iter().rev().take(5).rev() {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {:<7}", event.kind.label()),
                    task_event_style(event.kind),
                ),
                Span::styled(preview_text(&event.title, 28), Style::default().fg(WHITE)),
            ]));
        }
    } else {
        for message in app
            .messages
            .iter()
            .rev()
            .filter(|message| {
                matches!(
                    message.kind,
                    MsgKind::ToolCall { .. } | MsgKind::ToolResult { .. } | MsgKind::Error
                )
            })
            .take(5)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            let (label, color) = match &message.kind {
                MsgKind::ToolCall { name } => (format!("call {name}"), YELLOW),
                MsgKind::ToolResult { name } => (format!("done {name}"), CYAN),
                MsgKind::Error => ("error".into(), RED),
                _ => continue,
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {label:<14}"), Style::default().fg(color)),
                Span::styled(
                    preview_text(&message.text.replace('\n', " "), 24),
                    Style::default().fg(GRAY),
                ),
            ]));
        }
    }
    if let Some(review) = &app.editor.change_review {
        lines.extend([
            Line::from(""),
            Line::from(Span::styled(
                " CHANGE REVIEW",
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!(
                    " {}/{} hunks resolved",
                    review.resolved_count(),
                    review.hunk_count()
                ),
                Style::default().fg(YELLOW),
            )),
            Line::from(Span::styled(
                format!(" {} queued file(s)", app.pending_file_changes.len()),
                Style::default().fg(GRAY),
            )),
        ]);
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::LEFT)
                    .border_style(Style::default().fg(DIM_GREEN))
                    .bg(BG),
            )
            .bg(BG)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_completion_menu<M: CompletionModel>(
    frame: &mut ratatui::Frame,
    layout: EditorLayout,
    app: &App<M>,
) {
    if app.lsp_completions.is_empty() || layout.code.width < 24 || layout.code.height < 6 {
        return;
    }
    let visible_items = app.lsp_completions.len().min(8);
    let width = app
        .lsp_completions
        .iter()
        .take(40)
        .map(|item| {
            item.label.chars().count() + item.detail.as_deref().map_or(0, |d| d.chars().count() + 3)
        })
        .max()
        .unwrap_or(24)
        .clamp(24, 56) as u16
        + 2;
    let height = visible_items as u16 + 2;
    let line = &app.editor.buffer[app.editor.cursor.row];
    let visible_start = editor_char_boundary(line, app.editor.cursor.scroll_col);
    let cursor_col = editor_char_boundary(line, app.editor.cursor.col);
    let display_col = if cursor_col >= visible_start {
        UnicodeWidthStr::width(&line[visible_start..cursor_col]) as u16
    } else {
        0
    };
    let mut x = layout.code.x.saturating_add(6).saturating_add(display_col);
    if x.saturating_add(width) > layout.code.right() {
        x = layout.code.right().saturating_sub(width);
    }
    let cursor_y = layout.code.y.saturating_add(
        app.editor
            .cursor
            .row
            .saturating_sub(app.editor.cursor.scroll_row) as u16,
    );
    let y = if cursor_y.saturating_add(height + 1) < layout.code.bottom() {
        cursor_y + 1
    } else {
        cursor_y.saturating_sub(height)
    };
    let area = Rect::new(x, y, width.min(layout.code.width), height);
    frame.render_widget(Clear, area);
    let selected = app
        .lsp_completion_selected
        .min(app.lsp_completions.len().saturating_sub(1));
    let offset = selected
        .saturating_sub(visible_items / 2)
        .min(app.lsp_completions.len().saturating_sub(visible_items));
    let items = app
        .lsp_completions
        .iter()
        .skip(offset)
        .take(visible_items)
        .map(|item| {
            ListItem::new(Line::from(vec![
                Span::styled(item.label.clone(), Style::default().fg(WHITE)),
                Span::styled(
                    item.detail
                        .as_deref()
                        .map(|detail| format!(" · {}", preview_text(detail, 28)))
                        .unwrap_or_default(),
                    Style::default().fg(GRAY),
                ),
            ]))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    state.select(Some(selected.saturating_sub(offset)));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(format!(" {} ", app.lsp.server_name()))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(CYAN))
                    .bg(BG),
            )
            .highlight_style(Style::default().fg(BG).bg(GREEN)),
        area,
        &mut state,
    );
}

// ═══════════════════════════════════════════════════════════════
// EDITOR KEY HANDLING
// ═══════════════════════════════════════════════════════════════

fn context_language(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("rs") => "rust",
        Some("py") => "python",
        Some("js" | "mjs" | "cjs") => "javascript",
        Some("ts" | "mts" | "cts" | "tsx") => "typescript",
        Some("go") => "go",
        Some("c" | "h") => "c",
        Some("cc" | "cpp" | "cxx" | "hpp") => "cpp",
        Some("toml") => "toml",
        Some("json") => "json",
        Some("md") => "markdown",
        _ => "text",
    }
}

fn editor_context_block(context: &editor::CodeContext) -> String {
    const MAX_CONTEXT_CHARS: usize = 24_000;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let display_path = context
        .path
        .strip_prefix(&cwd)
        .unwrap_or(&context.path)
        .display();
    let mut chars = context.content.chars();
    let content = chars.by_ref().take(MAX_CONTEXT_CHARS).collect::<String>();
    let truncated = chars.next().is_some();
    format!(
        "[Code context]\nFile: {display_path}\nLines: {}-{}{}\n```{}\n{}{}\n```",
        context.start_line,
        context.end_line,
        context
            .symbol
            .as_ref()
            .map(|symbol| format!("\nSymbol: {symbol}"))
            .unwrap_or_default(),
        context_language(&context.path),
        content,
        if truncated {
            "\n[context truncated at 24000 characters]"
        } else {
            ""
        }
    )
}

fn stage_editor_context<M: CompletionModel>(app: &mut App<M>, durable: bool) {
    let Some(context) = app.editor.code_context() else {
        app.tab = Tab::Editor;
        app.editor.status_msg = "open a file before adding code context".into();
        return;
    };
    let context_block = editor_context_block(&context);
    let existing = app.input.lines().join("\n");
    let text = if durable {
        let objective = if existing.trim().is_empty() {
            "Implement and verify the requested work using this code context.".to_string()
        } else {
            existing
        };
        format!("/task {objective}\n\n{context_block}")
    } else if existing.trim().is_empty() {
        context_block
    } else {
        format!("{existing}\n\n{context_block}")
    };
    app.input = chat_input(text.split('\n').map(str::to_string).collect());
    app.input.move_cursor(CursorMove::Bottom);
    app.input.move_cursor(CursorMove::End);
    app.tab = Tab::Chat;
    app.status_line = if durable {
        " durable run objective staged · Alt+Enter to start".into()
    } else {
        " editor context added to Chat".into()
    };
}

fn queue_editor_memory_link<M: CompletionModel>(app: &mut App<M>) {
    let Some(context) = app.editor.code_context() else {
        app.tab = Tab::Editor;
        app.editor.status_msg = "open a file before pinning code to memory".into();
        return;
    };
    app.pending_code_link = Some(context);
    app.status_line = " creating navigable code memory".into();
}

async fn persist_pending_code_link<M: CompletionModel>(app: &mut App<M>) {
    let Some(context) = app.pending_code_link.take() else {
        return;
    };
    let path = normalize_path(&context.path);
    let path_text = path.to_string_lossy().to_string();
    let run_id = app
        .active_task
        .as_ref()
        .map(|task| task.id.as_str())
        .or_else(|| {
            app.task_view
                .as_ref()
                .filter(|view| view.status == TaskRunStatus::Running)
                .map(|view| view.id.as_str())
        });
    let related_fact_id = app.memory_console.selected_fact_id();
    let (start_line, end_line) = if context.selected {
        (context.start_line, context.end_line)
    } else {
        (context.focus_line, context.focus_line)
    };
    let snippet = if context.selected {
        context.content.clone()
    } else {
        let first = context.focus_line.saturating_sub(4);
        context
            .content
            .lines()
            .skip(first)
            .take(7)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let result = crate::tools::graph::store_code_location(crate::tools::graph::CodeLocation {
        path: &path_text,
        start_line,
        end_line,
        column: context.focus_column,
        symbol: context.symbol.as_deref(),
        snippet: &snippet,
        run_id,
        related_fact_id: related_fact_id.as_deref(),
    })
    .await;
    match result {
        Ok(id) => {
            app.memory_console.request_refresh();
            app.status_line = format!(" code context stored as {id}");
            app.editor.status_msg = "code location pinned to graph memory".into();
        }
        Err(error) => {
            app.status_line = format!(" code memory failed: {error}");
            app.editor.status_msg = format!("memory link failed: {error}");
        }
    }
}

fn sync_editor_lsp<M: CompletionModel>(app: &mut App<M>) {
    let Some(path) = app.editor.file_path.clone() else {
        if let Some(previous) = app.lsp_document_path.take() {
            app.lsp.close_document(&previous);
        }
        return;
    };
    if !app.lsp.supports_path(&path) {
        if let Some(previous) = app.lsp_document_path.take() {
            app.lsp.close_document(&previous);
        }
        app.lsp_status = format!(
            "{} ready · no server for .{}",
            app.lsp.server_name(),
            path.extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or("text")
        );
        return;
    }
    if app.lsp_document_path.as_deref() != Some(path.as_path()) {
        if let Some(previous) = app.lsp_document_path.take() {
            app.lsp.close_document(&previous);
        }
        app.lsp_document_path = Some(path.clone());
    }
    if let Err(error) = app.lsp.sync_document(&path, &app.editor.text()) {
        app.lsp_status = error;
    }
}

fn request_editor_completion<M: CompletionModel>(app: &mut App<M>) {
    let Some(path) = app.editor.file_path.clone() else {
        app.editor.status_msg = "open a file before requesting completion".into();
        return;
    };
    sync_editor_lsp(app);
    let line = &app.editor.buffer[app.editor.cursor.row];
    let position = lsp::Position {
        line: app.editor.cursor.row,
        character: lsp::utf16_column(line, app.editor.cursor.col),
    };
    match app.lsp.request_completion(&path, position) {
        Ok(()) => app.editor.status_msg = "requesting language-server completion".into(),
        Err(error) => {
            if app.editor.complete_at_cursor().is_none() {
                app.editor.status_msg = format!("completion unavailable: {error}");
            }
        }
    }
}

fn request_editor_definition<M: CompletionModel>(app: &mut App<M>) {
    app.tab = Tab::Editor;
    let Some(path) = app.editor.file_path.clone() else {
        app.editor.status_msg = "open a file before going to a definition".into();
        return;
    };
    sync_editor_lsp(app);
    let line = &app.editor.buffer[app.editor.cursor.row];
    let position = lsp::Position {
        line: app.editor.cursor.row,
        character: lsp::utf16_column(line, app.editor.cursor.col),
    };
    match app.lsp.request_definition(&path, position) {
        Ok(()) => app.editor.status_msg = "locating definition".into(),
        Err(error) => app.editor.status_msg = format!("definition unavailable: {error}"),
    }
}

fn handle_lsp_events<M: CompletionModel>(app: &mut App<M>) {
    for event in app.lsp.poll() {
        match event {
            lsp::Event::Ready { server } => {
                app.lsp_status = format!("{server} ready");
                sync_editor_lsp(app);
            }
            lsp::Event::Unavailable(error) => {
                app.lsp_status = error;
            }
            lsp::Event::Diagnostics { path, diagnostics } => {
                app.lsp_diagnostics
                    .insert(normalize_path(&path), diagnostics);
            }
            lsp::Event::Completion { path, items } => {
                if app
                    .editor
                    .file_path
                    .as_deref()
                    .is_some_and(|current| normalize_path(current) == normalize_path(&path))
                {
                    app.lsp_completions = items;
                    app.lsp_completion_selected = 0;
                    app.editor.status_msg = if app.lsp_completions.is_empty() {
                        "language server returned no completions".into()
                    } else {
                        format!("{} completion(s)", app.lsp_completions.len())
                    };
                }
            }
            lsp::Event::Definition(location) => match location {
                Some(location) => match app.editor.open(&location.path) {
                    Ok(()) => {
                        let row = location
                            .range
                            .start
                            .line
                            .min(app.editor.buffer.len().saturating_sub(1));
                        let col = lsp::byte_column(
                            &app.editor.buffer[row],
                            location.range.start.character,
                        );
                        app.editor.set_cursor(row, col);
                        reveal_editor_path(app, &location.path);
                        app.editor_tree_focused = false;
                        app.editor.status_msg =
                            format!("definition · {}:{}", location.path.display(), row + 1);
                        update_editor_scroll(app);
                        sync_editor_lsp(app);
                    }
                    Err(error) => {
                        app.editor.status_msg = format!("definition open failed: {error}")
                    }
                },
                None => app.editor.status_msg = "definition not found".into(),
            },
            lsp::Event::Error(error) => {
                app.lsp_status = error.clone();
                app.editor.status_msg = error;
            }
        }
    }
}

fn handle_editor_key<M: CompletionModel>(
    app: &mut App<M>,
    key: crossterm::event::KeyEvent,
) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};

    if !app.lsp_completions.is_empty() {
        match (key.modifiers, key.code) {
            (_, KeyCode::Esc) => {
                app.lsp_completions.clear();
                app.editor.status_msg = "completion cancelled".into();
                return true;
            }
            (_, KeyCode::Down | KeyCode::Tab) => {
                app.lsp_completion_selected =
                    (app.lsp_completion_selected + 1) % app.lsp_completions.len();
                return true;
            }
            (_, KeyCode::Up | KeyCode::BackTab) => {
                app.lsp_completion_selected = if app.lsp_completion_selected == 0 {
                    app.lsp_completions.len() - 1
                } else {
                    app.lsp_completion_selected - 1
                };
                return true;
            }
            (_, KeyCode::Enter) => {
                let completion = app.lsp_completions[app.lsp_completion_selected].clone();
                app.lsp_completions.clear();
                if let Err(error) = app.editor.apply_completion(&completion) {
                    app.editor.status_msg = format!("completion failed: {error}");
                }
                sync_editor_lsp(app);
                return true;
            }
            _ => app.lsp_completions.clear(),
        }
    }

    match (key.modifiers, key.code) {
        (KeyModifiers::ALT, KeyCode::Char('c')) => {
            stage_editor_context(app, false);
            return true;
        }
        (KeyModifiers::ALT, KeyCode::Char('r')) => {
            stage_editor_context(app, true);
            return true;
        }
        (KeyModifiers::ALT, KeyCode::Char('m')) => {
            queue_editor_memory_link(app);
            return true;
        }
        (KeyModifiers::ALT, KeyCode::Char('d')) | (KeyModifiers::CONTROL, KeyCode::Char(']')) => {
            request_editor_definition(app);
            return true;
        }
        (KeyModifiers::CONTROL, KeyCode::Char(' ')) => {
            request_editor_completion(app);
            return true;
        }
        _ => {}
    }

    if app.active_file_change.is_some() && app.editor.mode != editor::Mode::Command {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char('[') | KeyCode::Up) => {
                app.editor.move_review_hunk(-1);
            }
            (KeyModifiers::NONE, KeyCode::Char(']') | KeyCode::Down) => {
                app.editor.move_review_hunk(1);
            }
            (KeyModifiers::NONE, KeyCode::Char('a') | KeyCode::Enter) => {
                persist_review_decision(app, editor::ReviewDecision::Accepted, false);
            }
            (KeyModifiers::NONE, KeyCode::Char('r')) => {
                persist_review_decision(app, editor::ReviewDecision::Rejected, false);
            }
            (KeyModifiers::SHIFT, KeyCode::Char('A')) => {
                persist_review_decision(app, editor::ReviewDecision::Accepted, true);
            }
            (KeyModifiers::SHIFT, KeyCode::Char('R')) => {
                persist_review_decision(app, editor::ReviewDecision::Rejected, true);
            }
            (KeyModifiers::NONE, KeyCode::Char(':')) => {
                app.editor.mode = editor::Mode::Command;
                app.editor.cmd_buffer.clear();
            }
            _ => {
                app.editor.status_msg = "review locked · [/] hunk · a/r decide · A/R all".into();
            }
        }
        return true;
    }

    if app.editor.mode != editor::Mode::Command && key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('s') => {
                if let Err(error) = save_editor_buffer(app) {
                    app.editor.status_msg = format!("save error: {error}");
                }
                return true;
            }
            KeyCode::Char('z') => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    app.editor.redo();
                } else {
                    app.editor.undo();
                }
                update_editor_scroll(app);
                return true;
            }
            KeyCode::Char('y') | KeyCode::Char('r') => {
                app.editor.redo();
                update_editor_scroll(app);
                return true;
            }
            _ => {}
        }
    }

    if app.editor.mode == editor::Mode::Normal
        && key.modifiers == KeyModifiers::NONE
        && key.code == KeyCode::Tab
    {
        app.editor_tree_focused = !app.editor_tree_focused;
        app.editor.status_msg = if app.editor_tree_focused {
            "file tree focused".into()
        } else {
            "editor focused".into()
        };
        return true;
    }

    if app.editor_tree_focused && app.editor.mode == editor::Mode::Normal {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down) => {
                if app.file_tree_selected + 1 < app.file_tree_flat.len() {
                    app.file_tree_selected += 1;
                }
                return true;
            }
            (KeyModifiers::NONE, KeyCode::Char('k') | KeyCode::Up) => {
                app.file_tree_selected = app.file_tree_selected.saturating_sub(1);
                return true;
            }
            (KeyModifiers::NONE, KeyCode::Char('g') | KeyCode::Home) => {
                app.file_tree_selected = 0;
                return true;
            }
            (KeyModifiers::SHIFT, KeyCode::Char('G')) | (_, KeyCode::End) => {
                app.file_tree_selected = app.file_tree_flat.len().saturating_sub(1);
                return true;
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                activate_editor_tree_entry(app, app.file_tree_selected);
                return true;
            }
            (KeyModifiers::NONE, KeyCode::Char('l') | KeyCode::Right) => {
                if let Some((_, entry)) = app.file_tree_flat.get(app.file_tree_selected).cloned() {
                    if entry.is_dir {
                        editor::set_tree_expanded(&mut app.file_tree, &entry.path, true);
                        app.file_tree_flat = editor::flatten_tree(&app.file_tree, 0);
                    }
                }
                return true;
            }
            (KeyModifiers::NONE, KeyCode::Char('h') | KeyCode::Left) => {
                if let Some((_, entry)) = app.file_tree_flat.get(app.file_tree_selected).cloned() {
                    if entry.is_dir {
                        editor::set_tree_expanded(&mut app.file_tree, &entry.path, false);
                        app.file_tree_flat = editor::flatten_tree(&app.file_tree, 0);
                    }
                }
                return true;
            }
            (KeyModifiers::NONE, KeyCode::Char('i')) if app.editor.file_path.is_some() => {
                app.editor_tree_focused = false;
                app.editor.mode = editor::Mode::Insert;
                app.editor.status_msg = "insert mode".into();
                return true;
            }
            _ => {}
        }
    }

    let page_size = editor_page_size();
    let ed = &mut app.editor;
    match ed.mode {
        editor::Mode::Normal => match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char('h') | KeyCode::Left) => ed.move_left(),
            (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down) => ed.move_down(1),
            (KeyModifiers::NONE, KeyCode::Char('k') | KeyCode::Up) => ed.move_up(1),
            (KeyModifiers::NONE, KeyCode::Char('l') | KeyCode::Right) => ed.move_right(),
            (KeyModifiers::NONE, KeyCode::Char('w')) => ed.word_forward(),
            (KeyModifiers::NONE, KeyCode::Char('b')) => ed.word_backward(),
            (KeyModifiers::NONE, KeyCode::Char('0') | KeyCode::Home) => ed.move_to_start_of_line(),
            (KeyModifiers::NONE, KeyCode::Char('$') | KeyCode::End) => ed.move_to_end_of_line(),
            (KeyModifiers::NONE, KeyCode::Char('g')) => ed.move_to_first_line(),
            (KeyModifiers::SHIFT, KeyCode::Char('G')) => ed.move_to_last_line(),
            (_, KeyCode::PageUp) => ed.page_up(page_size),
            (_, KeyCode::PageDown) => ed.page_down(page_size),
            (KeyModifiers::NONE, KeyCode::Char('i')) => ed.mode = editor::Mode::Insert,
            (KeyModifiers::NONE, KeyCode::Char('a')) => {
                ed.move_right();
                ed.mode = editor::Mode::Insert;
            }
            (KeyModifiers::NONE, KeyCode::Char('o')) => {
                ed.move_to_end_of_line();
                ed.insert_newline();
                ed.mode = editor::Mode::Insert;
            }
            (KeyModifiers::NONE, KeyCode::Char('x')) => ed.delete_char(),
            (KeyModifiers::NONE, KeyCode::Char('d')) => ed.delete_line(),
            (KeyModifiers::NONE, KeyCode::Char('u')) => {
                ed.undo();
            }
            (KeyModifiers::NONE, KeyCode::Char('p')) => {
                ed.paste();
            }
            (KeyModifiers::NONE, KeyCode::Char('>')) => ed.indent_line(),
            (KeyModifiers::NONE, KeyCode::Char('<')) => ed.dedent_line(),
            (KeyModifiers::NONE, KeyCode::Char(':')) => {
                ed.mode = editor::Mode::Command;
                ed.cmd_buffer.clear();
            }
            (KeyModifiers::NONE, KeyCode::Char('/')) => {
                ed.mode = editor::Mode::Command;
                ed.cmd_buffer.clear();
                ed.cmd_buffer.push('/');
            }
            (KeyModifiers::NONE, KeyCode::Char('n')) => ed.search_next(),
            (KeyModifiers::SHIFT, KeyCode::Char('N')) => ed.search_prev(),
            (KeyModifiers::NONE, KeyCode::Char('v')) => ed.begin_selection(false),
            (KeyModifiers::SHIFT, KeyCode::Char('V')) => ed.begin_selection(true),
            _ => {}
        },
        editor::Mode::Insert => match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc) => {
                ed.clear_selection();
                ed.mode = editor::Mode::Normal;
            }
            (KeyModifiers::NONE, KeyCode::Left) => ed.move_left(),
            (KeyModifiers::NONE, KeyCode::Right) => ed.move_right(),
            (KeyModifiers::NONE, KeyCode::Up) => ed.move_up(1),
            (KeyModifiers::NONE, KeyCode::Down) => ed.move_down(1),
            (_, KeyCode::Home) => ed.move_to_start_of_line(),
            (_, KeyCode::End) => ed.move_to_end_of_line(),
            (_, KeyCode::PageUp) => ed.page_up(page_size),
            (_, KeyCode::PageDown) => ed.page_down(page_size),
            (KeyModifiers::NONE, KeyCode::Backspace) => ed.backspace(),
            (KeyModifiers::NONE, KeyCode::Delete) => ed.delete_char(),
            (KeyModifiers::NONE, KeyCode::Enter) => ed.insert_newline(),
            (KeyModifiers::NONE, KeyCode::Tab) => {
                for _ in 0..4 {
                    ed.insert_char(' ');
                }
            }
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => ed.insert_char(c),
            _ => {}
        },
        editor::Mode::Command => match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc) => {
                ed.mode = editor::Mode::Normal;
                ed.cmd_buffer.clear();
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                let cmd = ed.cmd_buffer.clone();
                ed.cmd_buffer.clear();
                ed.mode = editor::Mode::Normal;
                let _ = ed;
                exec_editor_command(app, &cmd);
                update_editor_scroll(app);
                return true;
            }
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                ed.cmd_buffer.pop();
            }
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                ed.cmd_buffer.push(c);
            }
            _ => {}
        },
        editor::Mode::Visual | editor::Mode::VisualLine => match (key.modifiers, key.code) {
            (_, KeyCode::Esc) => ed.clear_selection(),
            (KeyModifiers::NONE, KeyCode::Char('h') | KeyCode::Left) => ed.move_left(),
            (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down) => ed.move_down(1),
            (KeyModifiers::NONE, KeyCode::Char('k') | KeyCode::Up) => ed.move_up(1),
            (KeyModifiers::NONE, KeyCode::Char('l') | KeyCode::Right) => ed.move_right(),
            (KeyModifiers::NONE, KeyCode::Char('w')) => ed.word_forward(),
            (KeyModifiers::NONE, KeyCode::Char('b')) => ed.word_backward(),
            (KeyModifiers::NONE, KeyCode::Char('0') | KeyCode::Home) => {
                ed.move_to_start_of_line();
            }
            (KeyModifiers::NONE, KeyCode::Char('$') | KeyCode::End) => {
                ed.move_to_end_of_line();
            }
            (KeyModifiers::NONE, KeyCode::Char('g')) => ed.move_to_first_line(),
            (KeyModifiers::SHIFT, KeyCode::Char('G')) => ed.move_to_last_line(),
            (_, KeyCode::PageUp) => ed.page_up(page_size),
            (_, KeyCode::PageDown) => ed.page_down(page_size),
            (KeyModifiers::NONE, KeyCode::Char('y')) => {
                ed.copy_selection();
            }
            (KeyModifiers::NONE, KeyCode::Char('d') | KeyCode::Char('x')) => {
                ed.delete_selection();
            }
            (KeyModifiers::NONE, KeyCode::Char('v')) => {
                if ed.mode == editor::Mode::Visual {
                    ed.clear_selection();
                } else {
                    ed.mode = editor::Mode::Visual;
                }
            }
            (KeyModifiers::SHIFT, KeyCode::Char('V')) => {
                ed.mode = editor::Mode::VisualLine;
            }
            _ => {}
        },
    }
    update_editor_scroll(app);
    true
}

fn activate_editor_tree_entry<M: CompletionModel>(app: &mut App<M>, index: usize) {
    let Some((_, entry)) = app.file_tree_flat.get(index).cloned() else {
        return;
    };
    if entry.is_dir {
        editor::set_tree_expanded(&mut app.file_tree, &entry.path, !entry.expanded);
        app.file_tree_flat = editor::flatten_tree(&app.file_tree, 0);
        app.file_tree_selected = index.min(app.file_tree_flat.len().saturating_sub(1));
    } else {
        match app.editor.open(&entry.path) {
            Ok(()) => app.editor_tree_focused = false,
            Err(error) => app.editor.status_msg = format!("open error: {error}"),
        }
    }
}

fn editor_page_size() -> usize {
    memory_console_area()
        .map(editor_layout)
        .map_or(20, |layout| layout.code.height.saturating_sub(1) as usize)
        .max(1)
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

fn editor_byte_at_display_col(line: &str, start: usize, target: usize) -> usize {
    let start = editor_char_boundary(line, start);
    let mut width = 0;
    for (offset, character) in line[start..].char_indices() {
        let character_width = if character == '\t' {
            4
        } else {
            UnicodeWidthChar::width(character).unwrap_or(0).max(1)
        };
        if width + character_width > target {
            return start + offset;
        }
        width += character_width;
    }
    line.len()
}

fn editor_mouse_target<M: CompletionModel>(
    app: &App<M>,
    layout: EditorLayout,
    mouse: MouseEvent,
) -> Option<editor::Position> {
    if !rect_contains(layout.code, mouse.column, mouse.row) {
        return None;
    }
    let row = app
        .editor
        .cursor
        .scroll_row
        .saturating_add((mouse.row - layout.code.y) as usize)
        .min(app.editor.buffer.len().saturating_sub(1));
    let display_col = mouse.column.saturating_sub(layout.code.x.saturating_add(6)) as usize;
    let col = editor_byte_at_display_col(
        &app.editor.buffer[row],
        app.editor.cursor.scroll_col,
        display_col,
    );
    Some(editor::Position { row, col })
}

fn handle_editor_mouse<M: CompletionModel>(app: &mut App<M>, mouse: MouseEvent, area: Rect) {
    let layout = editor_layout(area);
    if rect_contains(layout.tree, mouse.column, mouse.row) {
        app.editor_tree_focused = true;
        let height = layout.tree.height as usize;
        let offset = editor_tree_offset(app, height);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let row = offset.saturating_add((mouse.row - layout.tree.y) as usize);
                if row < app.file_tree_flat.len() {
                    app.file_tree_selected = row;
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                let row = offset.saturating_add((mouse.row - layout.tree.y) as usize);
                if row < app.file_tree_flat.len() {
                    app.file_tree_selected = row;
                    activate_editor_tree_entry(app, row);
                }
            }
            MouseEventKind::ScrollUp => {
                app.file_tree_selected = app.file_tree_selected.saturating_sub(3);
            }
            MouseEventKind::ScrollDown => {
                app.file_tree_selected = app
                    .file_tree_selected
                    .saturating_add(3)
                    .min(app.file_tree_flat.len().saturating_sub(1));
            }
            _ => {}
        }
        return;
    }

    if !rect_contains(layout.code, mouse.column, mouse.row) {
        return;
    }
    app.editor_tree_focused = false;
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let anchor = app.editor.position();
            if let Some(target) = editor_mouse_target(app, layout, mouse) {
                if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                    if !matches!(
                        app.editor.mode,
                        editor::Mode::Visual | editor::Mode::VisualLine
                    ) {
                        app.editor.begin_selection(false);
                    }
                    app.editor_mouse_anchor = Some(anchor);
                    app.editor.set_cursor(target.row, target.col);
                } else {
                    app.editor.clear_selection();
                    app.editor.set_cursor(target.row, target.col);
                    app.editor_mouse_anchor = Some(target);
                }
                update_editor_scroll(app);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(target) = editor_mouse_target(app, layout, mouse) {
                if !matches!(
                    app.editor.mode,
                    editor::Mode::Visual | editor::Mode::VisualLine
                ) {
                    if let Some(anchor) = app.editor_mouse_anchor {
                        app.editor.set_cursor(anchor.row, anchor.col);
                    }
                    app.editor.begin_selection(false);
                }
                app.editor.set_cursor(target.row, target.col);
                update_editor_scroll(app);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            app.editor_mouse_anchor = None;
        }
        MouseEventKind::Down(MouseButton::Middle) => {
            if let Some(target) = editor_mouse_target(app, layout, mouse) {
                app.editor.set_cursor(target.row, target.col);
                app.editor.paste();
                update_editor_scroll(app);
            }
        }
        MouseEventKind::ScrollUp => {
            app.editor.cursor.scroll_row = app.editor.cursor.scroll_row.saturating_sub(3);
        }
        MouseEventKind::ScrollDown => {
            app.editor.cursor.scroll_row = app
                .editor
                .cursor
                .scroll_row
                .saturating_add(3)
                .min(app.editor.buffer.len().saturating_sub(1));
        }
        MouseEventKind::ScrollLeft => {
            app.editor.cursor.scroll_col = app.editor.cursor.scroll_col.saturating_sub(4);
        }
        MouseEventKind::ScrollRight => {
            app.editor.cursor.scroll_col = app.editor.cursor.scroll_col.saturating_add(4);
        }
        _ => {}
    }
}

fn exec_editor_command<M: CompletionModel>(app: &mut App<M>, cmd: &str) {
    let parts: Vec<&str> = cmd.splitn(2, ' ').collect();

    match parts[0] {
        "q" | "quit" => {
            if app.active_file_change.is_some() {
                app.editor.status_msg = "resolve the agent change review before exiting".into();
            } else if app.editor.dirty {
                app.editor.status_msg = "unsaved changes; use :w or :discard".into();
            } else {
                app.tab = Tab::Chat;
            }
        }
        "w" | "write" => {
            let result = if let Some(path) = parts.get(1) {
                app.editor.save_as(std::path::Path::new(path))
            } else {
                save_editor_buffer(app)
            };
            if let Err(e) = result {
                app.editor.status_msg = format!("save error: {e}");
            }
        }
        "wq" => {
            if let Err(e) = save_editor_buffer(app) {
                app.editor.status_msg = format!("save error: {e}");
            } else if app.editor.file_path.is_some()
                && !app.editor.dirty
                && app.active_file_change.is_none()
            {
                app.tab = Tab::Chat;
            }
        }
        "e" | "edit" => {
            let path = parts.get(1).copied().unwrap_or(".");
            if path == "." {
                load_editor_tree(app);
                app.editor_tree_focused = true;
                app.editor.status_msg = "file tree loaded".into();
            } else {
                let p = std::path::Path::new(path);
                if p.is_dir() {
                    app.file_tree = editor::build_file_tree(p, 0);
                    app.file_tree_flat = editor::flatten_tree(&app.file_tree, 0);
                    app.file_tree_selected = 0;
                    app.editor_tree_focused = true;
                    app.editor.status_msg = format!("browsing {}", p.display());
                } else {
                    match app.editor.open(p) {
                        Ok(()) => {
                            app.editor_tree_focused = false;
                        }
                        Err(e) => app.editor.status_msg = format!("open error: {e}"),
                    }
                }
            }
        }
        "diff" => {
            app.editor.compute_diff();
            app.editor.status_msg = "diff computed".into();
        }
        "chat" | "context" => stage_editor_context(app, false),
        "run" | "task" => stage_editor_context(app, true),
        "memory" | "pin" => queue_editor_memory_link(app),
        "definition" | "def" => request_editor_definition(app),
        "accept" => {
            persist_review_decision(app, editor::ReviewDecision::Accepted, false);
        }
        "reject" => {
            persist_review_decision(app, editor::ReviewDecision::Rejected, false);
        }
        "accept-all" => {
            persist_review_decision(app, editor::ReviewDecision::Accepted, true);
        }
        "reject-all" => {
            persist_review_decision(app, editor::ReviewDecision::Rejected, true);
        }
        "undo-change" => undo_reviewed_file_change(app),
        "apply" => {
            if let Err(e) = save_editor_buffer(app) {
                app.editor.status_msg = format!("save error: {e}");
            } else if !app.editor.dirty {
                app.editor.clear_diff();
            }
        }
        "discard" => {
            app.editor.revert();
        }
        _ if cmd.starts_with('/') => {
            app.editor.search(&cmd[1..]);
            app.editor.mode = editor::Mode::Normal;
        }
        _ => {
            if let Ok(n) = cmd.parse::<usize>() {
                app.editor.move_to_line(n.saturating_sub(1));
            } else {
                app.editor.status_msg = format!("unknown: {cmd}");
            }
        }
    }
}

fn load_editor_tree<M: CompletionModel>(app: &mut App<M>) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let expanded = expanded_editor_paths(&app.file_tree);
    app.file_tree = editor::build_file_tree(&cwd, 0);
    for path in expanded {
        editor::set_tree_expanded(&mut app.file_tree, &path, true);
    }
    app.file_tree_flat = editor::flatten_tree(&app.file_tree, 0);
    app.file_tree_selected = app
        .file_tree_selected
        .min(app.file_tree_flat.len().saturating_sub(1));
}

fn expanded_editor_paths(entries: &[editor::FileEntry]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in entries {
        if entry.expanded {
            paths.push(entry.path.clone());
        }
        paths.extend(expanded_editor_paths(&entry.children));
    }
    paths
}

fn reveal_editor_path<M: CompletionModel>(app: &mut App<M>, path: &std::path::Path) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut ancestors = path
        .ancestors()
        .skip(1)
        .filter(|ancestor| ancestor.starts_with(&cwd))
        .map(std::path::Path::to_path_buf)
        .collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        editor::set_tree_expanded(&mut app.file_tree, &ancestor, true);
    }
    app.file_tree_flat = editor::flatten_tree(&app.file_tree, 0);
    if let Some(index) = app
        .file_tree_flat
        .iter()
        .position(|(_, entry)| entry.path == path)
    {
        app.file_tree_selected = index;
    }
}

fn sync_agent_file_changes<M: CompletionModel>(app: &mut App<M>) {
    app.pending_file_changes
        .extend(crate::tools::file::take_file_changes());
    if app.active_file_change.is_some() || app.pending_file_changes.is_empty() {
        return;
    }

    load_editor_tree(app);
    if app.editor.dirty {
        app.status_line = format!(
            " {} agent file change(s) pending; save or discard the editor buffer to inspect",
            app.pending_file_changes.len()
        );
        return;
    }

    let current = app.editor.file_path.as_deref().map(normalize_path);
    let selected = app
        .pending_file_changes
        .iter()
        .position(|change| current.as_ref().is_some_and(|path| *path == change.path))
        .unwrap_or(0);
    let change = app.pending_file_changes.remove(selected);
    let Some(after) = change.after.as_deref() else {
        app.status_line = format!(
            " agent removed {}; binary review only",
            change.path.display()
        );
        app.active_file_change = Some(change);
        return;
    };
    let after_text = match std::str::from_utf8(after) {
        Ok(text) => text,
        Err(_) => {
            app.status_line = format!(
                " agent changed binary file {}; use :accept-all or :reject-all",
                change.path.display()
            );
            app.active_file_change = Some(change);
            return;
        }
    };
    let before_text = match change.before.as_deref().map(std::str::from_utf8) {
        Some(Ok(text)) => Some(text),
        Some(Err(_)) => {
            app.status_line = format!(
                " agent replaced binary file {}; use :accept-all or :reject-all",
                change.path.display()
            );
            app.active_file_change = Some(change);
            return;
        }
        None => None,
    };
    app.editor
        .begin_change_review(&change.path, before_text, after_text);
    reveal_editor_path(app, &change.path);
    app.editor_tree_focused = false;
    app.status_line = format!(
        " review agent change {} · {} queued",
        change.path.display(),
        app.pending_file_changes.len()
    );
    app.active_file_change = Some(change);
    sync_editor_lsp(app);
}

fn normalize_path(path: &std::path::Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn save_editor_buffer<M: CompletionModel>(app: &mut App<M>) -> std::io::Result<()> {
    let current = app.editor.file_path.as_deref().map(normalize_path);
    let pending = current.as_ref().is_some_and(|path| {
        app.pending_file_changes
            .iter()
            .any(|change| &change.path == path)
    });
    if pending {
        app.editor.status_msg =
            "agent changed this file while the buffer was dirty; use :discard to open review"
                .into();
        app.status_line = " save blocked by pending agent change".into();
        Ok(())
    } else {
        app.editor.save()
    }
}

fn persist_review_decision<M: CompletionModel>(
    app: &mut App<M>,
    decision: editor::ReviewDecision,
    all: bool,
) {
    if app.active_file_change.is_none() {
        app.editor.status_msg = "no agent change is awaiting review".into();
        return;
    }
    let decided = if all {
        app.editor.decide_all_review_hunks(decision)
    } else {
        app.editor.decide_review_hunk(decision)
    };
    if !decided {
        resolve_binary_review(app, decision == editor::ReviewDecision::Accepted);
        return;
    }
    let result = app.editor.review_result().unwrap_or_default().into_bytes();
    let result_exists = app.editor.review_result_exists();
    let expected = app
        .active_file_change
        .as_ref()
        .and_then(|change| change.after.as_deref());
    let path = app
        .active_file_change
        .as_ref()
        .map(|change| change.path.clone())
        .unwrap_or_default();
    if let Err(error) = crate::tools::file::write_review_result(
        &path,
        expected,
        result_exists.then_some(result.as_slice()),
    ) {
        app.editor.status_msg = format!("review conflict: {error}");
        app.status_line = format!(" {error}");
        return;
    }
    if let Some(change) = &mut app.active_file_change {
        change.after = result_exists.then_some(result.clone());
    }
    if app.editor.review_all_resolved() {
        finish_file_review(app, result_exists.then_some(result));
    }
}

fn resolve_binary_review<M: CompletionModel>(app: &mut App<M>, accept: bool) {
    let Some(mut change) = app.active_file_change.take() else {
        return;
    };
    let result = if accept {
        change.after.clone()
    } else {
        change.before.clone()
    };
    if let Err(error) = crate::tools::file::write_review_result(
        &change.path,
        change.after.as_deref(),
        result.as_deref(),
    ) {
        app.status_line = format!(" review conflict: {error}");
        app.active_file_change = Some(change);
        return;
    }
    change.after = result.clone();
    if accept && change.after != change.before {
        push_review_history(app, change.clone());
    }
    load_editor_tree(app);
    if let Some(bytes) = result {
        if let Ok(text) = String::from_utf8(bytes) {
            app.editor.finish_change_review(
                &text,
                if accept {
                    "agent change accepted"
                } else {
                    "agent change rejected"
                },
            );
        }
    } else {
        app.editor = Editor::new();
        app.editor.status_msg = "agent-created file rejected and removed".into();
    }
    app.status_line = if accept {
        " agent change accepted".into()
    } else {
        " agent change rejected".into()
    };
    sync_editor_lsp(app);
}

fn finish_file_review<M: CompletionModel>(app: &mut App<M>, result: Option<Vec<u8>>) {
    let Some(mut change) = app.active_file_change.take() else {
        return;
    };
    change.after = result.clone();
    let changed = change.after != change.before;
    if changed {
        push_review_history(app, change.clone());
    }
    load_editor_tree(app);
    match result {
        Some(bytes) => match String::from_utf8(bytes) {
            Ok(text) => app.editor.finish_change_review(
                &text,
                if changed {
                    "agent change review applied"
                } else {
                    "agent change rejected"
                },
            ),
            Err(_) => app.editor.status_msg = "binary review resolved".into(),
        },
        None => {
            app.editor = Editor::new();
            app.editor.status_msg = "agent-created file rejected and removed".into();
        }
    }
    app.status_line = format!(
        " review complete · {} change(s) queued",
        app.pending_file_changes.len()
    );
    sync_editor_lsp(app);
}

fn push_review_history<M: CompletionModel>(
    app: &mut App<M>,
    change: crate::tools::file::FileChange,
) {
    const HISTORY_LIMIT: usize = 50;
    app.reviewed_file_changes.push(change);
    if app.reviewed_file_changes.len() > HISTORY_LIMIT {
        app.reviewed_file_changes.remove(0);
    }
}

fn undo_reviewed_file_change<M: CompletionModel>(app: &mut App<M>) {
    let Some(change) = app.reviewed_file_changes.pop() else {
        app.editor.status_msg = "no accepted agent change to undo".into();
        return;
    };
    if let Err(error) = crate::tools::file::write_review_result(
        &change.path,
        change.after.as_deref(),
        change.before.as_deref(),
    ) {
        app.editor.status_msg = format!("undo conflict: {error}");
        app.reviewed_file_changes.push(change);
        return;
    }
    load_editor_tree(app);
    if change.before.is_some() {
        match app.editor.open(&change.path) {
            Ok(()) => {
                reveal_editor_path(app, &change.path);
                app.editor_tree_focused = false;
                app.editor.status_msg = "accepted agent change undone".into();
            }
            Err(error) => app.editor.status_msg = format!("undo reload failed: {error}"),
        }
    } else {
        app.editor = Editor::new();
        app.editor.status_msg = "accepted file creation undone".into();
    }
    sync_editor_lsp(app);
}

fn load_task_runs<M: CompletionModel>(app: &mut App<M>) -> anyhow::Result<()> {
    let selected_id = app.task_view.as_ref().map(|view| view.id.clone());
    app.task_runs = app.task_store.list()?;
    if app.task_runs.is_empty() {
        app.task_selected = 0;
        app.task_view = None;
        app.task_scroll = 0;
        return Ok(());
    }

    app.task_selected = selected_id
        .as_deref()
        .and_then(|id| app.task_runs.iter().position(|run| run.id == id))
        .unwrap_or(0)
        .min(app.task_runs.len().saturating_sub(1));
    refresh_selected_task(app)
}

fn select_task_run<M: CompletionModel>(app: &mut App<M>, id: &str) -> anyhow::Result<()> {
    let run = app.task_store.load(id)?;
    if let Some(index) = app.task_runs.iter().position(|summary| summary.id == id) {
        app.task_runs[index] = run.summary();
        app.task_selected = index;
    } else {
        app.task_runs.insert(0, run.summary());
        app.task_selected = 0;
    }
    app.task_view = Some(run.view());
    app.task_scroll = 0;
    Ok(())
}

fn refresh_selected_task<M: CompletionModel>(app: &mut App<M>) -> anyhow::Result<()> {
    let Some(id) = app
        .task_runs
        .get(app.task_selected)
        .map(|summary| summary.id.clone())
    else {
        app.task_view = None;
        app.task_selected = 0;
        return Ok(());
    };
    select_task_run(app, &id)
}

fn upsert_task_summary<M: CompletionModel>(app: &mut App<M>, view: &TaskView) {
    let summary = TaskRunSummary {
        id: view.id.clone(),
        objective: preview_text(&view.objective, 120),
        workspace: view.workspace.clone(),
        provider: view.provider.clone(),
        status: view.status,
        current_step: view.current_step,
        total_steps: view.steps.len(),
        updated_at: view.updated_at,
    };
    if let Some(index) = app.task_runs.iter().position(|run| run.id == summary.id) {
        app.task_runs[index] = summary;
    } else {
        app.task_runs.insert(0, summary);
        app.task_selected = 0;
    }
}

fn ensure_task_driver_available<M: CompletionModel>(app: &App<M>) -> anyhow::Result<()> {
    if app.active_task.is_some() {
        anyhow::bail!("another durable task is already active");
    }
    if app.thinking || app.active_run.is_some() {
        anyhow::bail!("finish or cancel the active chat run first");
    }
    if app.editor.dirty {
        anyhow::bail!("save or discard the editor's unsaved changes before starting a task");
    }
    if !app.provider_health.is_ready() {
        anyhow::bail!(app.provider_health.detail().to_string());
    }
    Ok(())
}

fn spawn_task_run<M: CompletionModel + 'static>(
    app: &mut App<M>,
    run: TaskRun,
    updates: mpsc::UnboundedSender<TaskNotification>,
) -> anyhow::Result<()> {
    ensure_task_driver_available(app)?;
    if !run.status.is_resumable() {
        anyhow::bail!("task run {} is already {}", run.id, run.status.label());
    }
    let workspace = std::env::current_dir()?.canonicalize()?;
    if workspace != run.workspace {
        anyhow::bail!(
            "run belongs to {}; restart UIntellAgent in that workspace to resume it",
            run.workspace.display()
        );
    }
    let approvals = app
        .confirm_state
        .clone()
        .ok_or_else(|| anyhow::anyhow!("tool confirmation service is unavailable"))?;
    let id = run.id.clone();
    let view = run.view();
    let store = app.task_store.clone();
    let agent = app.agent.clone();
    let (cancel, cancel_rx) = watch::channel(false);
    let error_updates = updates.clone();
    let driver_id = id.clone();
    let join = tokio::spawn(async move {
        if let Err(error) =
            crate::task_run::execute(store, run, agent, approvals, Some(updates), cancel_rx).await
        {
            let _ = error_updates.send(TaskNotification::DriverError {
                id: driver_id,
                error: error.to_string(),
            });
        }
    });

    upsert_task_summary(app, &view);
    if let Some(index) = app.task_runs.iter().position(|summary| summary.id == id) {
        app.task_selected = index;
    }
    app.task_view = Some(view);
    app.task_scroll = 0;
    app.active_task = Some(ActiveTask {
        id: id.clone(),
        cancel,
        join,
    });
    app.tab = Tab::Runs;
    app.status_line = format!(" task {id} started");
    Ok(())
}

fn start_task_run<M: CompletionModel + 'static>(
    app: &mut App<M>,
    objective: &str,
    updates: mpsc::UnboundedSender<TaskNotification>,
) -> anyhow::Result<()> {
    ensure_task_driver_available(app)?;
    let mut objective = objective.trim();
    let memory_writes = objective == "--remember" || objective.starts_with("--remember ");
    if memory_writes {
        objective = objective
            .strip_prefix("--remember")
            .unwrap_or(objective)
            .trim();
    }
    if objective.is_empty() {
        anyhow::bail!("usage: /task [--remember] <objective> or :new [--remember] <objective>");
    }
    let workspace = std::env::current_dir()?;
    let run = if memory_writes {
        app.task_store.create_with_memory(
            objective,
            &workspace,
            app.provider_label.clone(),
            true,
        )?
    } else {
        app.task_store
            .create(objective, &workspace, app.provider_label.clone())?
    };
    spawn_task_run(app, run, updates)
}

fn resume_selected_task<M: CompletionModel + 'static>(
    app: &mut App<M>,
    updates: mpsc::UnboundedSender<TaskNotification>,
) -> anyhow::Result<()> {
    ensure_task_driver_available(app)?;
    let id = app
        .task_view
        .as_ref()
        .map(|view| view.id.clone())
        .or_else(|| {
            app.task_runs
                .get(app.task_selected)
                .map(|summary| summary.id.clone())
        })
        .ok_or_else(|| anyhow::anyhow!("no durable task is selected"))?;
    let mut run = app.task_store.load(&id)?;
    if !run.status.is_resumable() {
        anyhow::bail!("task run {id} is already {}", run.status.label());
    }
    run.provider = app.provider_label.clone();
    app.task_store.save(&run)?;
    spawn_task_run(app, run, updates)
}

fn handle_task_notification<M: CompletionModel>(app: &mut App<M>, notification: TaskNotification) {
    match notification {
        TaskNotification::Updated(view) => {
            let selected = app
                .task_view
                .as_ref()
                .is_some_and(|selected| selected.id == view.id);
            let active = app
                .active_task
                .as_ref()
                .is_some_and(|task| task.id == view.id);
            let graph_changed = view.events.last().is_some_and(|event| {
                matches!(event.kind, crate::task_run::TaskEventKind::ToolResult)
                    && event.title.contains("graph_")
            });
            upsert_task_summary(app, &view);
            if selected || active {
                app.task_view = Some(view.clone());
            }
            if graph_changed {
                app.memory_console.request_refresh();
            }
            if active || (selected && view.status != TaskRunStatus::Running) {
                app.status_line = match view.status {
                    TaskRunStatus::Running => format!(
                        " task {} · step {}/{}",
                        view.id,
                        view.current_step.saturating_add(1).min(view.steps.len()),
                        view.steps.len()
                    ),
                    _ => format!(" task {} {}", view.id, view.status.label()),
                };
            }
        }
        TaskNotification::DriverError { id, error } => {
            if app
                .task_view
                .as_ref()
                .is_some_and(|selected| selected.id == id)
            {
                if let Ok(run) = app.task_store.load(&id) {
                    let view = run.view();
                    upsert_task_summary(app, &view);
                    app.task_view = Some(view);
                }
            }
            app.status_line = format!(" task {id} failed: {error}");
        }
    }
}

async fn reap_finished_task<M: CompletionModel>(app: &mut App<M>) {
    let finished = app
        .active_task
        .as_ref()
        .is_some_and(|task| task.join.is_finished());
    if !finished {
        return;
    }
    let Some(active) = app.active_task.take() else {
        return;
    };
    let id = active.id;
    if let Err(error) = active.join.await {
        app.status_line = format!(" task {id} driver stopped unexpectedly: {error}");
    }
}

fn cancel_task<M: CompletionModel>(app: &mut App<M>) -> bool {
    let Some(active) = &app.active_task else {
        return false;
    };
    let id = active.id.clone();
    let _ = active.cancel.send(true);
    if let Some(state) = &app.confirm_state {
        state.cancel_pending();
    }
    app.status_line = format!(" cancelling task {id}; checkpoint retained");
    true
}

async fn shutdown_active_task<M: CompletionModel>(app: &mut App<M>) {
    let Some(ActiveTask {
        id,
        cancel,
        mut join,
    }) = app.active_task.take()
    else {
        return;
    };
    let _ = cancel.send(true);
    if let Some(state) = &app.confirm_state {
        state.cancel_pending();
    }
    if tokio::time::timeout(Duration::from_secs(5), &mut join)
        .await
        .is_err()
    {
        join.abort();
        let _ = join.await;
        app.status_line = format!(" task {id} driver aborted after shutdown timeout");
    }
}

fn update_editor_scroll<M: CompletionModel>(app: &mut App<M>) {
    let ed = &mut app.editor;
    let layout = memory_console_area().map(editor_layout);
    let visible_h = layout
        .map_or(20, |layout| layout.code.height as usize)
        .max(1);
    let visible_w = layout
        .map_or(80, |layout| layout.code.width.saturating_sub(6) as usize)
        .max(1);

    if ed.cursor.row < ed.cursor.scroll_row {
        ed.cursor.scroll_row = ed.cursor.row;
    }
    if ed.cursor.row >= ed.cursor.scroll_row + visible_h {
        ed.cursor.scroll_row = ed.cursor.row.saturating_sub(visible_h - 1);
    }
    ed.cursor.scroll_row = ed.cursor.scroll_row.min(ed.buffer.len().saturating_sub(1));

    if ed.cursor.col < ed.cursor.scroll_col {
        ed.cursor.scroll_col = ed.cursor.col;
    }
    if ed.cursor.col >= ed.cursor.scroll_col.saturating_add(visible_w) {
        ed.cursor.scroll_col = ed.cursor.col.saturating_sub(visible_w - 1);
    }
    let line = &ed.buffer[ed.cursor.row];
    ed.cursor.scroll_col = editor_char_boundary(line, ed.cursor.scroll_col);
}

fn cancel_agent_run<M: CompletionModel>(app: &mut App<M>) -> bool {
    let was_running = app.thinking || app.active_run.is_some();
    if let Some(handle) = app.active_run.take() {
        handle.abort();
    }
    if let Some(state) = &app.confirm_state {
        state.cancel_pending();
    }
    if was_running {
        app.thinking = false;
        app.streaming_text.clear();
        app.active_tool = None;
        app.tool_args.clear();
        app.status_line = " agent run cancelled".into();
        app.add_msg(MsgKind::System, "Agent run cancelled".into());
    }
    was_running
}

fn submit_chat<M: CompletionModel>(app: &mut App<M>) -> Option<String> {
    let text = app.input.lines().join("\n");
    app.input = chat_input(Vec::new());
    if text.is_empty() {
        return None;
    }
    if let Some(task) = &app.active_task {
        let detail = format!(
            "Durable task {} is active. Cancel it with Ctrl+G before starting chat.",
            task.id
        );
        app.add_msg(MsgKind::Error, detail.clone());
        app.status_line = format!(" {detail}");
        return None;
    }
    if text.starts_with('/') {
        let q = handle_chat_cmd(app, &text);
        if q {
            return Some(String::new());
        }
        return None;
    }
    if !app.provider_health.is_ready() {
        let detail = app.provider_health.detail().to_string();
        app.add_msg(MsgKind::Error, detail.clone());
        app.status_line = format!(" {detail}");
        return None;
    }
    app.add_msg(MsgKind::User, text.clone());
    app.thinking = true;
    app.streaming_text.clear();
    app.active_tool = None;
    app.tool_args.clear();
    Some(text)
}

// ═══════════════════════════════════════════════════════════════
// CONFIRMATION DIALOG
// ═══════════════════════════════════════════════════════════════

fn render_confirm_dialog(
    frame: &mut ratatui::Frame,
    tool_name: &str,
    args: &str,
    reason: Option<&str>,
) {
    let area = frame.area();
    // Center a dialog box
    let dialog_w = 60u16.min(area.width.saturating_sub(4));
    let dialog_h = 8u16;
    let x = (area.width.saturating_sub(dialog_w)) / 2;
    let y = (area.height.saturating_sub(dialog_h)) / 2;
    let dialog_area = Rect {
        x,
        y,
        width: dialog_w,
        height: dialog_h,
    };

    frame.render_widget(Clear, dialog_area);

    let mut lines = vec![
        Line::from(Span::styled(
            " ⚠ Tool Confirmation Required",
            Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            " ─────────────────────────────",
            Style::default().fg(DIM_GREEN),
        )),
        Line::from(vec![
            Span::styled(" Tool: ", Style::default().fg(GRAY)),
            Span::styled(
                tool_name.to_string(),
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            ),
        ]),
    ];
    if let Some(r) = reason {
        lines.push(Line::from(vec![
            Span::styled("       ", Style::default()),
            Span::styled(r.to_string(), Style::default().fg(RED)),
        ]));
    }
    let args_preview = preview_text(args, 40);
    lines.push(Line::from(vec![
        Span::styled(" Args: ", Style::default().fg(GRAY)),
        Span::styled(args_preview, Style::default().fg(DIM_GREEN)),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " [y] Approve  [n] Deny  [a] Approve All  [Esc/Ctrl+G] Cancel run",
        Style::default().fg(GREEN),
    )));

    let dialog = Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(YELLOW))
                .bg(BG),
        )
        .bg(BG);

    frame.render_widget(dialog, dialog_area);
}

// ═══════════════════════════════════════════════════════════════
// ENTRY POINT
// ═══════════════════════════════════════════════════════════════

pub async fn run<M>(
    agent: Agent<M>,
    provider_label: &str,
    confirm_state: Arc<confirm::ConfirmState>,
    provider_health: ProviderHealth,
) -> anyhow::Result<()>
where
    M: CompletionModel + 'static,
{
    let mut terminal = ratatui::init();
    terminal.clear()?;
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
    let mut app = App::new(agent, provider_label, provider_health);
    app.confirm_state = Some(confirm_state);
    load_editor_tree(&mut app);
    if let Err(error) = load_task_runs(&mut app) {
        app.add_msg(
            MsgKind::Error,
            format!("Durable task history could not be loaded: {error}"),
        );
    }
    let workspace_state = match load_workspace_state() {
        Ok(state) => state,
        Err(error) => {
            app.add_msg(
                MsgKind::Error,
                format!("Workspace state was ignored: {error}"),
            );
            None
        }
    };
    if let Some(state) = &workspace_state {
        restore_workspace_state(&mut app, state);
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let (task_updates_tx, task_updates_rx) = mpsc::unbounded_channel();

    app.memory_console.initialize().await;
    if let Some(state) = &workspace_state {
        app.memory_console.restore_state(&state.graph);
    }

    let result = run_loop(
        &mut terminal,
        &mut app,
        tx,
        rx,
        task_updates_tx,
        task_updates_rx,
    )
    .await;
    shutdown_active_task(&mut app).await;

    if app
        .messages
        .iter()
        .any(|message| matches!(message.kind, MsgKind::User | MsgKind::Agent))
    {
        let _ = save_session(
            app.session_name.as_deref().unwrap_or("autosave"),
            &app.messages.iter().cloned().collect::<Vec<_>>(),
        );
    }
    let workspace_result = save_workspace_state(&app);
    let mouse_result = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    let raw_mode_result = crossterm::terminal::disable_raw_mode();
    ratatui::restore();
    mouse_result?;
    raw_mode_result?;
    if let Err(error) = workspace_result {
        eprintln!("Workspace state warning: {error}");
    }
    result
}

fn memory_console_area() -> Option<Rect> {
    let (width, height) = crossterm::terminal::size().ok()?;
    (width > 0 && height >= 5).then(|| Rect::new(0, 3, width, height.saturating_sub(4)))
}

async fn run_loop<M: CompletionModel + 'static>(
    terminal: &mut DefaultTerminal,
    app: &mut App<M>,
    tx: mpsc::UnboundedSender<StreamEvent>,
    mut rx: mpsc::UnboundedReceiver<StreamEvent>,
    task_updates_tx: mpsc::UnboundedSender<TaskNotification>,
    mut task_updates_rx: mpsc::UnboundedReceiver<TaskNotification>,
) -> anyhow::Result<()> {
    loop {
        app.memory_console.tick();
        handle_lsp_events(app);
        sync_agent_file_changes(app);
        persist_pending_code_link(app).await;

        reap_finished_task(app).await;
        while let Ok(notification) = task_updates_rx.try_recv() {
            handle_task_notification(app, notification);
        }
        if let Some(objective) = app.pending_task_objective.take() {
            if let Err(error) = start_task_run(app, &objective, task_updates_tx.clone()) {
                let detail = format!("Could not start durable task: {error}");
                app.status_line = format!(" {detail}");
                app.add_msg(MsgKind::Error, detail);
            }
        }

        // Check for pending tool confirmations (from the hook)
        if let Some(cs) = app.confirm_state.clone() {
            if let Some(req) = cs.take_pending() {
                // Show confirmation dialog — loop until user responds
                let response_tx = req.response_tx;
                let tool_name = req.tool_name.clone();
                let args = req.args.clone();
                let reason = req.reason.clone();

                let approved = loop {
                    // Render with confirmation overlay
                    terminal.draw(|f| {
                        ui(f, app);
                        render_confirm_dialog(f, &tool_name, &args, reason.as_deref());
                    })?;

                    if !event::poll(Duration::from_millis(100))? {
                        continue;
                    }
                    if let Event::Key(key) = event::read()? {
                        match (key.modifiers, key.code) {
                            (_, KeyCode::Char('y') | KeyCode::Char('Y')) => break true,
                            (_, KeyCode::Char('n') | KeyCode::Char('N')) => break false,
                            (_, KeyCode::Char('a') | KeyCode::Char('A')) => {
                                cs.approve_all();
                                break true;
                            }
                            (KeyModifiers::CONTROL, KeyCode::Char('c') | KeyCode::Char('g'))
                            | (_, KeyCode::Esc) => {
                                if app.active_task.is_some() {
                                    cancel_task(app);
                                } else {
                                    cancel_agent_run(app);
                                }
                                break false;
                            }
                            _ => {}
                        }
                    }
                };

                cs.respond(approved, response_tx);
                if app.thinking {
                    app.status_line = if approved {
                        format!(" approved: {tool_name}")
                    } else {
                        format!(" denied: {tool_name}")
                    };
                }
            }
        }

        terminal.draw(|f| ui(f, app))?;

        // Drain stream events
        while let Ok(ev) = rx.try_recv() {
            match ev {
                StreamEvent::TextDelta(t) => {
                    app.streaming_text.push_str(&t);
                    app.scroll = 0;
                }
                StreamEvent::ToolCall { name } => {
                    if let Some(pt) = app.active_tool.take() {
                        app.add_msg(MsgKind::ToolCall { name: pt }, app.tool_args.clone());
                    }
                    app.active_tool = Some(name);
                    app.tool_args.clear();
                    app.streaming_text.clear();
                }
                StreamEvent::ToolArgs(a) => {
                    app.tool_args.push_str(&a);
                }
                StreamEvent::ToolResult { name, result } => {
                    let active_tool = app.active_tool.take();
                    let tool_name = active_tool.clone().unwrap_or(name);
                    if active_tool.is_some() {
                        app.add_msg(
                            MsgKind::ToolCall {
                                name: tool_name.clone(),
                            },
                            app.tool_args.clone(),
                        );
                    }
                    app.add_msg(
                        MsgKind::ToolResult {
                            name: tool_name.clone(),
                        },
                        result,
                    );
                    if tool_name.starts_with("graph_") {
                        app.memory_console.request_refresh();
                    }
                    app.tool_args.clear();
                    app.scroll = 0;
                }
                StreamEvent::Usage {
                    input_tokens,
                    output_tokens,
                } => {
                    app.status_line = format!(" ↑{input_tokens} ↓{output_tokens}");
                }
                StreamEvent::Done(text) => {
                    app.active_run = None;
                    if !text.is_empty() {
                        let response =
                            if !app.streaming_text.is_empty() && app.streaming_text != text {
                                app.streaming_text.clone()
                            } else {
                                text
                            };
                        app.add_msg(MsgKind::Agent, response.clone());
                        app.chat_history.push(RigMessage::assistant(response));
                    }
                    if let Some(tn) = app.active_tool.take() {
                        app.add_msg(MsgKind::ToolCall { name: tn }, app.tool_args.clone());
                    }
                    app.streaming_text.clear();
                    app.tool_args.clear();
                    app.thinking = false;
                    app.scroll = 0;
                }
                StreamEvent::Error(e) => {
                    app.active_run = None;
                    if let Some(tn) = app.active_tool.take() {
                        app.add_msg(MsgKind::ToolCall { name: tn }, app.tool_args.clone());
                    }
                    app.add_msg(MsgKind::Error, format!("[!] {e}"));
                    app.streaming_text.clear();
                    app.tool_args.clear();
                    app.thinking = false;
                }
            }
        }

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let ev = event::read()?;
        match ev {
            Event::Key(key) => {
                if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
                    if cancel_agent_run(app) {
                        while rx.try_recv().is_ok() {}
                        continue;
                    }
                    if cancel_task(app) {
                        continue;
                    }
                    if app.active_file_change.is_some() && !app.exit_armed {
                        app.exit_armed = true;
                        app.tab = Tab::Editor;
                        app.editor.status_msg =
                            "pending agent change review; A accepts all, R rejects all, Ctrl+C again forces exit"
                                .into();
                        app.status_line = " exit blocked by pending change review".into();
                        continue;
                    }
                    if app.editor.dirty && !app.exit_armed {
                        app.exit_armed = true;
                        app.tab = Tab::Editor;
                        app.editor.status_msg =
                            "unsaved changes; use :w or :discard, Ctrl+C again to force exit"
                                .into();
                        app.status_line = " exit blocked by unsaved editor changes".into();
                        continue;
                    }
                    break;
                }
                app.exit_armed = false;
                if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('g') {
                    if cancel_agent_run(app) {
                        while rx.try_recv().is_ok() {}
                    } else {
                        cancel_task(app);
                    }
                    continue;
                }

                if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('p') {
                    if app.mode == Mode::Palette {
                        app.mode = Mode::Normal;
                        app.cmd_buffer.clear();
                    } else {
                        app.mode = Mode::Palette;
                        app.cmd_buffer.clear();
                        app.palette_selected = 0;
                    }
                    continue;
                }

                if app.mode == Mode::Palette {
                    let filtered_len = palette_items(&app.cmd_buffer).len();
                    match (key.modifiers, key.code) {
                        (_, KeyCode::Esc) => {
                            app.mode = Mode::Normal;
                            app.cmd_buffer.clear();
                        }
                        (KeyModifiers::NONE, KeyCode::Down) => {
                            app.palette_selected = app
                                .palette_selected
                                .saturating_add(1)
                                .min(filtered_len.saturating_sub(1));
                        }
                        (KeyModifiers::NONE, KeyCode::Up) => {
                            app.palette_selected = app.palette_selected.saturating_sub(1);
                        }
                        (_, KeyCode::PageDown) => {
                            app.palette_selected = app
                                .palette_selected
                                .saturating_add(8)
                                .min(filtered_len.saturating_sub(1));
                        }
                        (_, KeyCode::PageUp) => {
                            app.palette_selected = app.palette_selected.saturating_sub(8);
                        }
                        (_, KeyCode::Home) => app.palette_selected = 0,
                        (_, KeyCode::End) => app.palette_selected = filtered_len.saturating_sub(1),
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            if let Some(item) = palette_items(&app.cmd_buffer)
                                .get(app.palette_selected)
                                .cloned()
                            {
                                execute_palette_action(app, item.action, task_updates_tx.clone());
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Backspace) => {
                            app.cmd_buffer.pop();
                            app.palette_selected = 0;
                        }
                        (modifiers, KeyCode::Char(character))
                            if command_modifiers_accept_text(modifiers) =>
                        {
                            app.cmd_buffer.push(character);
                            app.palette_selected = 0;
                        }
                        _ => {}
                    }
                    continue;
                }

                // Command/search/insert mode (global)
                if app.mode != Mode::Normal {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            app.mode = Mode::Normal;
                            app.cmd_buffer.clear();
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            let cmd = app.cmd_buffer.clone();
                            app.cmd_buffer.clear();
                            let was_cmd = app.mode == Mode::Command;
                            app.mode = Mode::Normal;
                            if was_cmd {
                                if exec_command(app, &cmd, task_updates_tx.clone()).await {
                                    break;
                                }
                            } else if !cmd.is_empty() {
                                search_in_tab(app, &cmd);
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Backspace) => {
                            app.cmd_buffer.pop();
                        }
                        (modifiers, KeyCode::Char(c))
                            if command_modifiers_accept_text(modifiers) =>
                        {
                            app.cmd_buffer.push(c);
                        }
                        _ => {}
                    }
                    continue;
                }

                let active_view_accepts_text = app.tab == Tab::Chat
                    || (app.tab == Tab::Editor && app.editor.mode != editor::Mode::Normal);
                if let Some(tab) = workspace_shortcut(app.tab, active_view_accepts_text, key) {
                    app.tab = tab;
                    if tab == Tab::Editor {
                        update_editor_scroll(app);
                    }
                    continue;
                }

                match app.tab {
                    Tab::Chat => {
                        if let Some(mode) = chat_mode_shortcut(key) {
                            app.mode = mode;
                            app.cmd_buffer.clear();
                            continue;
                        }
                        match (key.modifiers, key.code) {
                            (KeyModifiers::ALT, KeyCode::Enter) => {
                                if app.thinking {
                                    continue;
                                }
                                if let Some(p) = submit_chat(app) {
                                    if p.is_empty() {
                                        break;
                                    }
                                    let history = app.chat_history.clone();
                                    let prompt_text = p.clone();
                                    app.chat_history.push(RigMessage::user(prompt_text.clone()));
                                    app.active_run = Some(spawn_stream(
                                        app.agent.clone(),
                                        prompt_text,
                                        history,
                                        tx.clone(),
                                    ));
                                }
                            }
                            (KeyModifiers::NONE, KeyCode::Esc) if app.thinking => {
                                cancel_agent_run(app);
                                while rx.try_recv().is_ok() {}
                            }
                            (_, KeyCode::PageUp) => {
                                app.scroll += 5;
                            }
                            (_, KeyCode::PageDown) => {
                                app.scroll = app.scroll.saturating_sub(5);
                            }
                            (KeyModifiers::NONE, KeyCode::Up) => {
                                if app.input.cursor().0 == 0 {
                                    app.scroll += 1;
                                    continue;
                                }
                            }
                            (KeyModifiers::NONE, KeyCode::Down) => {
                                if app.input.cursor().0 >= app.input.lines().len().saturating_sub(1)
                                {
                                    app.scroll = app.scroll.saturating_sub(1);
                                    continue;
                                }
                            }
                            (KeyModifiers::CONTROL, KeyCode::Char('s')) => {
                                let n = app
                                    .session_name
                                    .clone()
                                    .unwrap_or_else(|| "quicksave".into());
                                if save_session(
                                    &n,
                                    &app.messages.iter().cloned().collect::<Vec<_>>(),
                                )
                                .is_ok()
                                {
                                    app.status_line = format!(" saved:{n}");
                                }
                                continue;
                            }
                            (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
                                app.messages.clear();
                                app.scroll = 0;
                                continue;
                            }
                            _ => {
                                let _ = app.input.input(key);
                            }
                        }
                    }
                    Tab::Memory => {
                        if let Some(area) = memory_console_area() {
                            match app.memory_console.handle_event(Event::Key(key), area).await {
                                GraphConsoleAction::Exit => {
                                    app.tab = Tab::Chat;
                                    app.status_line = " returned from Memory".into();
                                }
                                GraphConsoleAction::OpenCode { path, line, column } => {
                                    match app.editor.open(&path) {
                                        Ok(()) => {
                                            let row = line
                                                .saturating_sub(1)
                                                .min(app.editor.buffer.len().saturating_sub(1));
                                            let col = column
                                                .saturating_sub(1)
                                                .min(app.editor.buffer[row].len());
                                            app.editor.set_cursor(row, col);
                                            reveal_editor_path(app, &path);
                                            app.editor_tree_focused = false;
                                            app.editor.status_msg = format!(
                                                "opened from memory · {}:{line}",
                                                path.display()
                                            );
                                            app.tab = Tab::Editor;
                                            update_editor_scroll(app);
                                            sync_editor_lsp(app);
                                        }
                                        Err(error) => {
                                            app.status_line =
                                                format!(" memory code location failed: {error}")
                                        }
                                    }
                                }
                                GraphConsoleAction::Continue => {}
                            }
                        }
                    }
                    Tab::Tools => match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down) => {
                            if app.tool_selected + 1 < crate::tools::CATALOG.len() {
                                app.tool_selected += 1;
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Char('k') | KeyCode::Up) => {
                            app.tool_selected = app.tool_selected.saturating_sub(1);
                        }
                        (KeyModifiers::NONE, KeyCode::Home | KeyCode::Char('g')) => {
                            app.tool_selected = 0;
                        }
                        (_, KeyCode::End) | (KeyModifiers::SHIFT, KeyCode::Char('G')) => {
                            app.tool_selected = crate::tools::CATALOG.len().saturating_sub(1);
                        }
                        (KeyModifiers::NONE, KeyCode::Char(':')) => {
                            app.mode = Mode::Command;
                            app.cmd_buffer.clear();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('/')) => {
                            app.mode = Mode::Search;
                            app.cmd_buffer.clear();
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            if let Some(tool) = crate::tools::CATALOG.get(app.tool_selected) {
                                app.mode = Mode::Command;
                                app.cmd_buffer = format!("run {} {}", tool.name, tool.example);
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Char('c')) => app.tool_output.clear(),
                        _ => {}
                    },
                    Tab::Editor => {
                        handle_editor_key(app, key);
                        if app.tab == Tab::Editor {
                            sync_editor_lsp(app);
                        }
                    }
                    Tab::Runs => match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down) => {
                            if app.task_selected + 1 < app.task_runs.len() {
                                app.task_selected += 1;
                                if let Err(error) = refresh_selected_task(app) {
                                    app.status_line = format!(" task refresh failed: {error}");
                                }
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Char('k') | KeyCode::Up) => {
                            if app.task_selected > 0 {
                                app.task_selected -= 1;
                                if let Err(error) = refresh_selected_task(app) {
                                    app.status_line = format!(" task refresh failed: {error}");
                                }
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Home | KeyCode::Char('g')) => {
                            app.task_selected = 0;
                            if let Err(error) = refresh_selected_task(app) {
                                app.status_line = format!(" task refresh failed: {error}");
                            }
                        }
                        (_, KeyCode::End) | (KeyModifiers::SHIFT, KeyCode::Char('G')) => {
                            app.task_selected = app.task_runs.len().saturating_sub(1);
                            if let Err(error) = refresh_selected_task(app) {
                                app.status_line = format!(" task refresh failed: {error}");
                            }
                        }
                        (_, KeyCode::PageUp) => app.task_scroll = app.task_scroll.saturating_add(5),
                        (_, KeyCode::PageDown) => {
                            app.task_scroll = app.task_scroll.saturating_sub(5)
                        }
                        (KeyModifiers::NONE, KeyCode::Char('n')) => {
                            app.mode = Mode::Command;
                            app.cmd_buffer = "new ".into();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('r')) => {
                            if let Err(error) = resume_selected_task(app, task_updates_tx.clone()) {
                                app.status_line = format!(" task resume failed: {error}");
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Char('c')) => {
                            if !cancel_task(app) {
                                app.status_line = " no durable task is active".into();
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            if let Err(error) = refresh_selected_task(app) {
                                app.status_line = format!(" task refresh failed: {error}");
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Char(':')) => {
                            app.mode = Mode::Command;
                            app.cmd_buffer.clear();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('/')) => {
                            app.mode = Mode::Search;
                            app.cmd_buffer.clear();
                        }
                        _ => {}
                    },
                }
            }
            Event::Mouse(_) if app.mode == Mode::Palette => {}
            Event::Mouse(mouse) => match app.tab {
                Tab::Memory => {
                    if let Some(area) = memory_console_area() {
                        app.memory_console
                            .handle_event(Event::Mouse(mouse), area)
                            .await;
                    }
                }
                Tab::Editor => {
                    if let Some(area) = memory_console_area() {
                        handle_editor_mouse(app, mouse, area);
                        sync_editor_lsp(app);
                    }
                }
                _ => {}
            },
            Event::Resize(..) => {}
            _ => {}
        }
    }
    cancel_agent_run(app);
    cancel_task(app);
    Ok(())
}

async fn exec_command<M: CompletionModel + 'static>(
    app: &mut App<M>,
    cmd: &str,
    task_updates: mpsc::UnboundedSender<TaskNotification>,
) -> bool {
    let parts: Vec<&str> = cmd.splitn(2, ' ').collect();
    if matches!(parts[0], "q" | "quit") {
        if app.active_file_change.is_some() {
            app.tab = Tab::Editor;
            app.editor.status_msg = "resolve the agent change review before exiting".into();
            app.status_line = " exit blocked by pending change review".into();
            return false;
        }
        if app.editor.dirty {
            app.tab = Tab::Editor;
            app.editor.status_msg = "unsaved changes; use :w or :discard before exiting".into();
            app.status_line = " exit blocked by unsaved editor changes".into();
            return false;
        }
        return true;
    }
    match (app.tab, parts[0]) {
        (Tab::Chat, _) => {
            app.add_msg(
                MsgKind::System,
                "Chat mode — use /commands, not :commands".to_string(),
            );
        }
        (Tab::Memory, _) => {
            app.status_line = " Memory commands are handled by the graph console".into();
        }
        (Tab::Tools, "run") => run_tool_from_tui(app, parts.get(1).copied(), false).await,
        (Tab::Tools, "run!") => run_tool_from_tui(app, parts.get(1).copied(), true).await,
        (Tab::Tools, "clear") => app.tool_output.clear(),
        (Tab::Runs, "new") => {
            if let Err(error) =
                start_task_run(app, parts.get(1).copied().unwrap_or(""), task_updates)
            {
                app.status_line = format!(" task start failed: {error}");
            }
        }
        (Tab::Runs, "resume" | "retry") => {
            if let Err(error) = resume_selected_task(app, task_updates) {
                app.status_line = format!(" task resume failed: {error}");
            }
        }
        (Tab::Runs, "refresh" | "list") => {
            if let Err(error) = load_task_runs(app) {
                app.status_line = format!(" task refresh failed: {error}");
            } else {
                app.status_line = format!(" {} durable task run(s)", app.task_runs.len());
            }
        }
        (Tab::Runs, "open") => {
            let id = parts.get(1).copied().unwrap_or("").trim();
            if id.is_empty() {
                app.status_line = " usage: :open <run-id>".into();
            } else if let Err(error) = select_task_run(app, id) {
                app.status_line = format!(" task open failed: {error}");
            }
        }
        (Tab::Runs, "cancel") => {
            if !cancel_task(app) {
                app.status_line = " no durable task is active".into();
            }
        }
        (Tab::Runs, "help") => {
            app.status_line =
                " :new <objective> · :resume · :refresh · :open <id> · :cancel".into();
        }
        _ => {
            app.status_line = format!(" Unknown: {cmd}");
        }
    }
    false
}

async fn run_tool_from_tui<M: CompletionModel>(
    app: &mut App<M>,
    invocation: Option<&str>,
    confirmed: bool,
) {
    let Some(invocation) = invocation else {
        app.tool_output = "Usage: :run <tool> <json>".into();
        return;
    };
    let mut parts = invocation.trim().splitn(2, ' ');
    let name = parts.next().unwrap_or_default();
    let args = parts.next().unwrap_or("{}").trim();

    if !crate::tools::CATALOG.iter().any(|tool| tool.name == name) {
        app.tool_output = format!("Unknown tool: {name}");
        return;
    }
    if let Err(error) = serde_json::from_str::<Value>(args) {
        app.tool_output = format!("Invalid JSON arguments: {error}");
        return;
    }

    match crate::permissions::permission_for_tool(name, args) {
        crate::permissions::PermissionResult::Denied(reason) => {
            app.tool_output = format!("PERMISSION DENIED: {reason}");
            return;
        }
        crate::permissions::PermissionResult::Confirm(reason) if !confirmed => {
            app.tool_output = format!(
                "CONFIRMATION REQUIRED: {reason}\n\nRun the same call with :run! to confirm it."
            );
            return;
        }
        crate::permissions::PermissionResult::Confirm(_) => {
            crate::permissions::record_approval(name, args);
        }
        crate::permissions::PermissionResult::Allowed => {}
    }

    app.status_line = format!(" running {name}");
    app.tool_output = format!("Running {name}...\n\nArgs: {args}");
    match crate::tools::execute_named(name, args).await {
        Ok(output) => {
            app.tool_output = format!("{name}\n\n{output}");
            app.status_line = format!(" {name} complete");
            if name.starts_with("graph_") {
                app.memory_console.request_refresh();
            }
        }
        Err(error) => {
            app.tool_output = format!("{name} failed\n\n{error}");
            app.status_line = format!(" {name} failed");
        }
    }
}

fn search_in_tab<M: CompletionModel>(app: &mut App<M>, query: &str) {
    let lower = query.to_lowercase();
    match app.tab {
        Tab::Chat => {
            // Search messages
            if let Some((i, _)) = app
                .messages
                .iter()
                .enumerate()
                .find(|(_, m)| m.text.to_lowercase().contains(&lower))
            {
                // Can't easily scroll to it, just report
                app.status_line = format!(" Found in message #{}", i);
            } else {
                app.status_line = format!(" No match: {query}");
            }
        }
        Tab::Memory => app.status_line = " Use / inside the graph console to search memory".into(),
        Tab::Tools => {
            if let Some(position) = crate::tools::CATALOG.iter().position(|tool| {
                tool.name.to_lowercase().contains(&lower)
                    || tool.description.to_lowercase().contains(&lower)
            }) {
                app.tool_selected = position;
                app.status_line = format!(" Found: {}", crate::tools::CATALOG[position].name);
            } else {
                app.status_line = format!(" No match: {query}");
            }
        }
        Tab::Editor => {
            // Search in editor buffer
            app.editor.search(query);
        }
        Tab::Runs => {
            if let Some(position) = app.task_runs.iter().position(|run| {
                run.id.to_lowercase().contains(&lower)
                    || run.objective.to_lowercase().contains(&lower)
                    || run.provider.to_lowercase().contains(&lower)
                    || run
                        .workspace
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&lower)
            }) {
                app.task_selected = position;
                if let Err(error) = refresh_selected_task(app) {
                    app.status_line = format!(" task search failed: {error}");
                } else {
                    app.status_line = format!(" Found: {}", app.task_runs[position].id);
                }
            } else {
                app.status_line = format!(" No task match: {query}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn memory_reserves_plain_number_keys_for_graph_views() {
        assert_eq!(
            workspace_shortcut(
                Tab::Memory,
                false,
                key(KeyCode::Char('1'), KeyModifiers::NONE)
            ),
            None
        );
        assert_eq!(
            workspace_shortcut(
                Tab::Memory,
                false,
                key(KeyCode::Char('1'), KeyModifiers::ALT)
            ),
            Some(Tab::Chat)
        );
    }

    #[test]
    fn plain_workspace_shortcuts_remain_available_on_non_text_views() {
        assert_eq!(
            workspace_shortcut(
                Tab::Tools,
                false,
                key(KeyCode::Char('2'), KeyModifiers::NONE)
            ),
            Some(Tab::Memory)
        );
        assert_eq!(
            workspace_shortcut(
                Tab::Editor,
                true,
                key(KeyCode::Char('1'), KeyModifiers::NONE)
            ),
            None
        );
        assert_eq!(
            workspace_shortcut(Tab::Chat, false, key(KeyCode::Char('5'), KeyModifiers::ALT)),
            Some(Tab::Runs)
        );
    }

    #[test]
    fn chat_keeps_paths_code_and_numbers_in_the_text_input() {
        let mut input = chat_input(Vec::new());
        for character in "cd /Uintellagent:123".chars() {
            let event = key(KeyCode::Char(character), KeyModifiers::NONE);
            assert!(chat_mode_shortcut(event).is_none());
            assert_eq!(workspace_shortcut(Tab::Chat, true, event), None);
            let _ = input.input(event);
        }

        assert_eq!(input.lines(), &["cd /Uintellagent:123".to_string()]);
        assert!(matches!(
            chat_mode_shortcut(key(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            Some(Mode::Search)
        ));
        assert_eq!(
            workspace_shortcut(Tab::Chat, true, key(KeyCode::Char('2'), KeyModifiers::ALT)),
            Some(Tab::Memory)
        );
    }

    #[test]
    fn editor_layout_only_allocates_an_inspector_on_wide_terminals() {
        let narrow = editor_layout(Rect::new(0, 0, 100, 30));
        assert!(narrow.inspector.is_none());
        assert!(narrow.code.width > 0);

        let wide = editor_layout(Rect::new(0, 0, 160, 45));
        assert!(wide.inspector.is_some());
        assert!(wide.code.width > 0);
    }

    #[test]
    fn workspace_state_round_trips_with_graph_state() {
        let state = WorkspaceState {
            active_tab: "editor".into(),
            editor_path: Some(PathBuf::from("/tmp/example.rs")),
            editor_row: 42,
            editor_col: 7,
            task_run_id: Some("run-1234".into()),
            ..WorkspaceState::default()
        };
        let encoded = serde_json::to_vec(&state).unwrap();
        let restored: WorkspaceState = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(restored.version, WORKSPACE_STATE_VERSION);
        assert_eq!(restored.active_tab, "editor");
        assert_eq!(restored.editor_path, state.editor_path);
        assert_eq!(restored.editor_row, 42);
        assert_eq!(restored.editor_col, 7);
        assert_eq!(restored.task_run_id.as_deref(), Some("run-1234"));
    }

    #[test]
    fn shared_command_line_accepts_shifted_characters() {
        assert!(command_modifiers_accept_text(KeyModifiers::SHIFT));
        assert!(command_modifiers_accept_text(KeyModifiers::NONE));
        assert!(!command_modifiers_accept_text(KeyModifiers::CONTROL));
    }

    #[test]
    fn command_palette_filters_across_titles_and_descriptions() {
        let query = palette_items("memory query");
        assert!(query.iter().any(|item| item.title == "Memory: Query"));
        assert!(query
            .iter()
            .all(|item| format!("{} {}", item.title, item.detail)
                .to_ascii_lowercase()
                .contains("memory")));

        let tools = palette_items("sandboxed python");
        assert_eq!(tools.len(), 1);
        assert!(matches!(tools[0].action, PaletteAction::SelectTool(_)));
        assert_eq!(tools[0].title, "Tool: code_exec");
    }
}
