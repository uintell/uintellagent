// UIntell Graph Operations Console
//
// A standalone SurrealDB manager for visual graph editing, structured browsing,
// safe SurrealQL execution, graph analytics, exports, and undoable deletion.

use crate::knowledge_graph::{
    compute_layout, sql_string, valid_edge_id, valid_fact_id, valid_label, Dataset, Edge, Fact,
    GraphAnalytics, GraphFilter, GraphLoadOptions, GraphRepository, GraphSnapshot, LayoutConfig,
    SpatialIndex, Viewport,
};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use futures::StreamExt;
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
    DefaultTerminal,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tui_textarea::CursorMove;

const GREEN: Color = Color::Rgb(0x33, 0xFF, 0x33);
const DIM_GREEN: Color = Color::Rgb(0x1A, 0x7A, 0x1A);
const BG: Color = Color::Rgb(0x05, 0x05, 0x05);
const CYAN: Color = Color::Rgb(0x00, 0xCC, 0xCC);
const YELLOW: Color = Color::Rgb(0xCC, 0xCC, 0x00);
const MAGENTA: Color = Color::Rgb(0xCC, 0x00, 0xCC);
const RED: Color = Color::Rgb(0xFF, 0x33, 0x33);
const GRAY: Color = Color::Rgb(0x66, 0x66, 0x66);
const DARK_GRAY: Color = Color::Rgb(0x33, 0x33, 0x33);
const WHITE: Color = Color::Rgb(0xCC, 0xCC, 0xCC);

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Graph,
    Explorer,
    Query,
    Analytics,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Graph, Tab::Explorer, Tab::Query, Tab::Analytics];

    fn title(self) -> &'static str {
        match self {
            Self::Graph => "Graph",
            Self::Explorer => "Explorer",
            Self::Query => "Query",
            Self::Analytics => "Analytics",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    Command,
    Search,
    QueryEdit,
    Create,
    Edit,
    Confirm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueryFocus {
    Templates,
    History,
    Editor,
    Results,
}

impl QueryFocus {
    fn next(self, reverse: bool) -> Self {
        match (self, reverse) {
            (Self::Templates, false) | (Self::Editor, true) => Self::History,
            (Self::History, false) | (Self::Results, true) => Self::Editor,
            (Self::Editor, false) | (Self::Templates, true) => Self::Results,
            (Self::Results, false) | (Self::History, true) => Self::Templates,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ExportSnapshot {
    version: u32,
    namespace: String,
    database: String,
    exported_at_unix: u64,
    #[serde(default)]
    total_facts: u64,
    #[serde(default)]
    partial: bool,
    datasets: Vec<Dataset>,
    facts: Vec<Fact>,
    edges: Vec<Edge>,
}

#[derive(Serialize, Deserialize)]
struct DeletedFacts {
    facts: Vec<Fact>,
    edges: Vec<Edge>,
}

#[derive(Clone, Serialize, Deserialize)]
struct SavedView {
    name: String,
    dataset_filter: String,
    type_filter: String,
    center_x: f64,
    center_y: f64,
    zoom: f64,
}

#[derive(Clone)]
enum ConfirmAction {
    Facts(Vec<String>),
    Edge(String),
    Dataset(String),
    Query(String),
}

#[derive(Clone, Debug)]
enum GraphJobKind {
    Refresh,
    Layout,
    Query { sql: String, allow_write: bool },
}

impl GraphJobKind {
    fn label(&self) -> &'static str {
        match self {
            Self::Refresh => "refresh",
            Self::Layout => "layout",
            Self::Query { .. } => "query",
        }
    }
}

struct QueryJobResult {
    rows: Vec<Value>,
    allow_write: bool,
    refreshed: Option<GraphSnapshot>,
    refresh_error: Option<String>,
}

enum GraphJobPayload {
    Refresh(Result<GraphSnapshot, String>),
    Layout(Result<Vec<(String, f64, f64)>, String>),
    Query(Result<QueryJobResult, String>),
}

enum GraphJobEvent {
    Progress {
        id: u64,
        percent: u8,
        phase: &'static str,
    },
    Finished {
        id: u64,
        payload: GraphJobPayload,
    },
}

struct ActiveGraphJob {
    id: u64,
    kind: GraphJobKind,
    started: Instant,
    percent: u8,
    phase: &'static str,
    abort: AbortHandle,
}

struct App {
    tab: Tab,
    mode: Mode,
    facts: Vec<Fact>,
    edges: Vec<Edge>,
    datasets: Vec<Dataset>,
    total_facts: u64,
    load_limit: usize,
    spatial_index: SpatialIndex,
    fact_lookup: HashMap<String, usize>,
    analytics: GraphAnalytics,
    saved_views: Vec<SavedView>,
    selected: usize,
    edge_selected: usize,
    marked: BTreeSet<String>,
    dataset_filter: String,
    type_filter: String,
    relation_type: String,
    link_source: Option<String>,
    dragging: bool,
    lasso_start: Option<(u16, u16)>,
    lasso_end: Option<(u16, u16)>,
    zoom: f64,
    center_x: f64,
    center_y: f64,
    status: String,
    command: String,
    query_editor: tui_textarea::TextArea<'static>,
    query_focus: QueryFocus,
    query_output: String,
    query_scroll: usize,
    query_history: VecDeque<String>,
    history_selected: usize,
    template_selected: usize,
    form_values: [String; 5],
    form_field: usize,
    confirm: Option<ConfirmAction>,
    job_sequence: u64,
    active_job: Option<ActiveGraphJob>,
    last_job: Option<GraphJobKind>,
    job_tx: mpsc::UnboundedSender<GraphJobEvent>,
    job_rx: mpsc::UnboundedReceiver<GraphJobEvent>,
}

impl App {
    fn new() -> Self {
        let (job_tx, job_rx) = mpsc::unbounded_channel();
        Self {
            tab: Tab::Graph,
            mode: Mode::Normal,
            facts: Vec::new(),
            edges: Vec::new(),
            datasets: vec![Dataset::default_dataset()],
            total_facts: 0,
            load_limit: GraphLoadOptions::default().fact_limit,
            spatial_index: SpatialIndex::default(),
            fact_lookup: HashMap::new(),
            analytics: GraphAnalytics::default(),
            saved_views: Vec::new(),
            selected: 0,
            edge_selected: 0,
            marked: BTreeSet::new(),
            dataset_filter: "all".into(),
            type_filter: "all".into(),
            relation_type: "relates_to".into(),
            link_source: None,
            dragging: false,
            lasso_start: None,
            lasso_end: None,
            zoom: 1.0,
            center_x: 0.5,
            center_y: 0.5,
            status: "connecting".into(),
            command: String::new(),
            query_editor: new_query_editor(QUERY_TEMPLATES[0].1),
            query_focus: QueryFocus::Templates,
            query_output: String::new(),
            query_scroll: 0,
            query_history: VecDeque::new(),
            history_selected: 0,
            template_selected: 0,
            form_values: Default::default(),
            form_field: 0,
            confirm: None,
            job_sequence: 0,
            active_job: None,
            last_job: None,
            job_tx,
            job_rx,
        }
    }
}

fn new_query_editor(sql: &str) -> tui_textarea::TextArea<'static> {
    let lines = if sql.is_empty() {
        vec![String::new()]
    } else {
        sql.split('\n').map(str::to_string).collect()
    };
    let mut editor = tui_textarea::TextArea::new(lines);
    editor.set_style(Style::default().fg(WHITE).bg(BG));
    editor.set_cursor_style(Style::default().fg(BG).bg(GREEN));
    editor.set_line_number_style(Style::default().fg(DARK_GRAY).bg(BG));
    editor.set_tab_length(4);
    editor
}

async fn db(sql: &str) -> Result<Vec<Value>, String> {
    GraphRepository::query(sql).await
}

async fn db_transaction(sql: &str) -> Result<Vec<Value>, String> {
    GraphRepository::transaction(sql).await
}

async fn refresh(app: &mut App) -> Result<(), String> {
    let snapshot = GraphRepository::load(load_options(app)).await?;
    apply_snapshot(app, snapshot);
    Ok(())
}

fn apply_snapshot(app: &mut App, snapshot: GraphSnapshot) {
    let selected_id = app.facts.get(app.selected).map(|fact| fact.id.clone());
    app.facts = snapshot.facts;
    app.edges = snapshot.edges;
    app.datasets = snapshot.datasets;
    app.total_facts = snapshot.total_facts;
    rebuild_graph_caches(app);
    app.marked
        .retain(|id| app.facts.iter().any(|fact| &fact.id == id));
    if app.dataset_filter != "all"
        && !app
            .datasets
            .iter()
            .any(|dataset| dataset.name == app.dataset_filter)
    {
        app.dataset_filter = "all".into();
    }
    app.selected = selected_id
        .and_then(|id| app.facts.iter().position(|fact| fact.id == id))
        .filter(|index| fact_visible(app, &app.facts[*index]))
        .or_else(|| visible_indices(app).first().copied())
        .unwrap_or(0);
    app.edge_selected = 0;
}

fn rebuild_graph_caches(app: &mut App) {
    app.spatial_index.rebuild(&app.facts);
    app.fact_lookup = app
        .facts
        .iter()
        .enumerate()
        .map(|(index, fact)| (fact.id.clone(), index))
        .collect();
    app.analytics = GraphAnalytics::compute(&app.facts, &app.edges);
}

fn load_options(app: &App) -> GraphLoadOptions {
    GraphLoadOptions {
        fact_limit: app.load_limit,
        edge_limit_per_type: app.load_limit.saturating_mul(4).clamp(5_000, 500_000),
        ..GraphLoadOptions::default()
    }
}

fn start_graph_job(app: &mut App, kind: GraphJobKind) {
    if let Some(active) = &app.active_job {
        app.status = format!(
            "{} is already running at {}%; Esc/Ctrl+G cancels it",
            active.kind.label(),
            active.percent
        );
        return;
    }
    if matches!(kind, GraphJobKind::Layout) && visible_indices(app).is_empty() {
        app.status = "nothing to lay out".into();
        return;
    }

    app.job_sequence = app.job_sequence.wrapping_add(1);
    let id = app.job_sequence;
    let tx = app.job_tx.clone();
    let options = load_options(app);
    let handle = match &kind {
        GraphJobKind::Refresh => tokio::spawn(async move {
            let _ = tx.send(GraphJobEvent::Progress {
                id,
                percent: 10,
                phase: "loading graph snapshot",
            });
            let result = GraphRepository::load(options).await;
            let _ = tx.send(GraphJobEvent::Finished {
                id,
                payload: GraphJobPayload::Refresh(result),
            });
        }),
        GraphJobKind::Layout => {
            let facts = app.facts.clone();
            let edges = app.edges.clone();
            let indices = visible_indices(app);
            tokio::spawn(async move {
                let _ = tx.send(GraphJobEvent::Progress {
                    id,
                    percent: 10,
                    phase: "computing topology",
                });
                let facts_for_layout = facts.clone();
                let layout = tokio::task::spawn_blocking(move || {
                    compute_layout(&facts_for_layout, &edges, &indices, LayoutConfig::default())
                })
                .await
                .map_err(|error| format!("layout worker failed: {error}"));
                let result = match layout {
                    Ok(updates) if updates.is_empty() => Ok(Vec::new()),
                    Ok(updates) => {
                        let _ = tx.send(GraphJobEvent::Progress {
                            id,
                            percent: 75,
                            phase: "persisting positions",
                        });
                        match GraphRepository::persist_positions(&facts, &updates).await {
                            Ok(()) => Ok(updates
                                .iter()
                                .map(|update| (facts[update.index].id.clone(), update.x, update.y))
                                .collect()),
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                };
                let _ = tx.send(GraphJobEvent::Finished {
                    id,
                    payload: GraphJobPayload::Layout(result),
                });
            })
        }
        GraphJobKind::Query { sql, allow_write } => {
            let sql = sql.clone();
            let allow_write = *allow_write;
            tokio::spawn(async move {
                let _ = tx.send(GraphJobEvent::Progress {
                    id,
                    percent: 15,
                    phase: "executing SurrealQL",
                });
                let result = match GraphRepository::query(&sql).await {
                    Ok(rows) => {
                        let (refreshed, refresh_error) = if allow_write {
                            let _ = tx.send(GraphJobEvent::Progress {
                                id,
                                percent: 75,
                                phase: "refreshing changed graph",
                            });
                            match GraphRepository::load(options).await {
                                Ok(snapshot) => (Some(snapshot), None),
                                Err(error) => (None, Some(error)),
                            }
                        } else {
                            (None, None)
                        };
                        Ok(QueryJobResult {
                            rows,
                            allow_write,
                            refreshed,
                            refresh_error,
                        })
                    }
                    Err(error) => Err(error),
                };
                let _ = tx.send(GraphJobEvent::Finished {
                    id,
                    payload: GraphJobPayload::Query(result),
                });
            })
        }
    };

    let label = kind.label();
    app.last_job = Some(kind.clone());
    app.active_job = Some(ActiveGraphJob {
        id,
        kind,
        started: Instant::now(),
        percent: 0,
        phase: "queued",
        abort: handle.abort_handle(),
    });
    app.status = format!("{label} queued; Esc/Ctrl+G cancels");
}

fn drain_graph_jobs(app: &mut App) {
    while let Ok(event) = app.job_rx.try_recv() {
        match event {
            GraphJobEvent::Progress { id, percent, phase } => {
                if let Some(active) = app.active_job.as_mut().filter(|active| active.id == id) {
                    active.percent = percent.min(99);
                    active.phase = phase;
                }
            }
            GraphJobEvent::Finished { id, payload } => {
                if app.active_job.as_ref().map(|active| active.id) != Some(id) {
                    continue;
                }
                app.active_job = None;
                match payload {
                    GraphJobPayload::Refresh(Ok(snapshot)) => {
                        apply_snapshot(app, snapshot);
                        app.status = "database refreshed in background".into();
                    }
                    GraphJobPayload::Refresh(Err(error)) => {
                        app.status = format!("refresh failed: {error}");
                    }
                    GraphJobPayload::Layout(Ok(positions)) => {
                        for (id, x, y) in &positions {
                            if let Some(index) = app.fact_lookup.get(id).copied() {
                                app.facts[index].graph_x = *x;
                                app.facts[index].graph_y = *y;
                            }
                        }
                        rebuild_graph_caches(app);
                        app.status = if positions.is_empty() {
                            "all visible nodes are pinned".into()
                        } else {
                            format!("auto-layout persisted {} nodes", positions.len())
                        };
                    }
                    GraphJobPayload::Layout(Err(error)) => {
                        app.status = format!("layout failed: {error}");
                    }
                    GraphJobPayload::Query(Ok(result)) => {
                        app.query_output = result
                            .rows
                            .iter()
                            .map(|row| format!("{row:#}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        app.tab = Tab::Query;
                        if let Some(snapshot) = result.refreshed {
                            apply_snapshot(app, snapshot);
                        }
                        app.status = if let Some(error) = result.refresh_error {
                            format!("query completed; graph refresh failed: {error}")
                        } else if result.allow_write {
                            "mutating query completed and graph refreshed".into()
                        } else {
                            "query completed".into()
                        };
                    }
                    GraphJobPayload::Query(Err(error)) => {
                        app.query_output = format!("ERROR\n\n{error}");
                        app.tab = Tab::Query;
                        app.status = "query failed".into();
                    }
                }
            }
        }
    }
}

fn cancel_graph_job(app: &mut App) {
    let Some(active) = app.active_job.take() else {
        app.status = "no background operation is running".into();
        return;
    };
    active.abort.abort();
    app.status = format!("cancelled {}", active.kind.label());
}

fn retry_graph_job(app: &mut App) {
    let Some(kind) = app.last_job.clone() else {
        app.status = "no background operation to retry".into();
        return;
    };
    if matches!(
        kind,
        GraphJobKind::Query {
            allow_write: true,
            ..
        }
    ) {
        app.status = "mutating queries are never retried automatically".into();
        return;
    }
    start_graph_job(app, kind);
}

fn load_more(app: &mut App, amount: Option<&str>, all: bool) {
    const MAX_INTERACTIVE_FACTS: usize = 100_000;
    if let Some(active) = &app.active_job {
        app.status = format!("{} is still running; cancel or wait", active.kind.label());
        return;
    }
    let requested = if all {
        usize::try_from(app.total_facts)
            .unwrap_or(MAX_INTERACTIVE_FACTS)
            .min(MAX_INTERACTIVE_FACTS)
    } else {
        let increment = match amount.map(str::trim).filter(|value| !value.is_empty()) {
            Some(value) => match value.parse::<usize>() {
                Ok(value) if value > 0 => value,
                _ => {
                    app.status = "usage: :load-more [positive-count]".into();
                    return;
                }
            },
            None => 2_000,
        };
        app.load_limit
            .saturating_add(increment)
            .min(MAX_INTERACTIVE_FACTS)
    };
    if requested <= app.facts.len()
        && app.total_facts <= u64::try_from(app.facts.len()).unwrap_or(u64::MAX)
    {
        app.status = "all knowledge units are already loaded".into();
        return;
    }
    app.load_limit = requested.max(1);
    start_graph_job(app, GraphJobKind::Refresh);
}

fn mutation_blocked(app: &mut App) -> bool {
    let Some(active) = &app.active_job else {
        return false;
    };
    app.status = format!(
        "wait for {} or press Esc to cancel it before changing the graph",
        active.kind.label()
    );
    true
}

fn fact_visible(app: &App, fact: &Fact) -> bool {
    GraphFilter::from_labels(&app.dataset_filter, &app.type_filter).matches(fact)
}

fn visible_indices(app: &App) -> Vec<usize> {
    GraphFilter::from_labels(&app.dataset_filter, &app.type_filter).indices(&app.facts)
}

fn selected_fact(app: &App) -> Option<&Fact> {
    app.facts
        .get(app.selected)
        .filter(|fact| fact_visible(app, fact))
}

fn connected_edges(app: &App) -> Vec<usize> {
    let Some(fact) = selected_fact(app) else {
        return Vec::new();
    };
    app.edges
        .iter()
        .enumerate()
        .filter_map(|(index, edge)| {
            (edge.from_id == fact.id || edge.to_id == fact.id).then_some(index)
        })
        .collect()
}

fn select_offset(app: &mut App, offset: isize) {
    let visible = visible_indices(app);
    if visible.is_empty() {
        return;
    }
    let current = visible
        .iter()
        .position(|index| *index == app.selected)
        .unwrap_or(0);
    let next = if offset.is_negative() {
        current.saturating_sub(offset.unsigned_abs())
    } else {
        (current + offset as usize).min(visible.len() - 1)
    };
    app.selected = visible[next];
    app.edge_selected = 0;
}

fn select_endpoint(app: &mut App, last: bool) {
    let visible = visible_indices(app);
    if let Some(index) = if last {
        visible.last()
    } else {
        visible.first()
    } {
        app.selected = *index;
        app.edge_selected = 0;
    }
}

fn cycle_dataset(app: &mut App, reverse: bool) {
    let mut names = vec!["all".to_string()];
    names.extend(app.datasets.iter().map(|dataset| dataset.name.clone()));
    names.dedup();
    let current = names
        .iter()
        .position(|name| name == &app.dataset_filter)
        .unwrap_or(0);
    let next = if reverse {
        current.checked_sub(1).unwrap_or(names.len() - 1)
    } else {
        (current + 1) % names.len()
    };
    app.dataset_filter = names[next].clone();
    select_endpoint(app, false);
    app.status = format!("dataset filter: {}", app.dataset_filter);
}

fn cycle_type(app: &mut App, reverse: bool) {
    let mut names = vec!["all".to_string()];
    names.extend(app.facts.iter().map(|fact| fact.fact_type.clone()));
    names.sort();
    names.dedup();
    let current = names
        .iter()
        .position(|name| name == &app.type_filter)
        .unwrap_or(0);
    let next = if reverse {
        current.checked_sub(1).unwrap_or(names.len() - 1)
    } else {
        (current + 1) % names.len()
    };
    app.type_filter = names[next].clone();
    select_endpoint(app, false);
    app.status = format!("type filter: {}", app.type_filter);
}

fn fit_indices(app: &mut App, indices: &[usize], label: &str) {
    let positions = indices
        .iter()
        .filter_map(|index| app.facts.get(*index))
        .map(|fact| (fact.graph_x, fact.graph_y))
        .collect::<Vec<_>>();
    if positions.is_empty() {
        app.status = format!("nothing to fit for {label}");
        return;
    }
    let (mut min_x, mut min_y) = positions[0];
    let (mut max_x, mut max_y) = positions[0];
    for (x, y) in positions.iter().skip(1) {
        min_x = min_x.min(*x);
        min_y = min_y.min(*y);
        max_x = max_x.max(*x);
        max_y = max_y.max(*y);
    }
    app.center_x = ((min_x + max_x) / 2.0).clamp(0.0, 1.0);
    app.center_y = ((min_y + max_y) / 2.0).clamp(0.0, 1.0);
    let span = (max_x - min_x).max(max_y - min_y).max(0.04);
    app.zoom = (0.78 / span).clamp(0.5, 6.0);
    app.status = format!(
        "fit {} {label} unit(s) at {:.1}x",
        positions.len(),
        app.zoom
    );
}

fn fit_selection(app: &mut App, all_visible: bool) {
    let indices = if all_visible {
        visible_indices(app)
    } else if app.marked.is_empty() {
        selected_fact(app)
            .and_then(|fact| app.fact_lookup.get(&fact.id).copied())
            .into_iter()
            .collect()
    } else {
        app.marked
            .iter()
            .filter_map(|id| app.fact_lookup.get(id).copied())
            .collect()
    };
    fit_indices(
        app,
        &indices,
        if all_visible { "visible" } else { "selected" },
    );
}

fn type_color(fact_type: &str) -> Color {
    match fact_type {
        "preference" => YELLOW,
        "finding" => CYAN,
        "user_detail" => MAGENTA,
        "decision" => Color::LightBlue,
        "error" => RED,
        "fix" => Color::LightGreen,
        _ => GREEN,
    }
}

fn trunc(value: &str, max: usize) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

fn ui(frame: &mut ratatui::Frame, area: Rect, app: &App, embedded: bool) {
    frame.buffer_mut().set_style(area, Style::default().bg(BG));
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
    render_tabs(frame, layout[0], app);
    match app.tab {
        Tab::Graph => render_graph(frame, layout[1], app),
        Tab::Explorer => render_explorer(frame, layout[1], app),
        Tab::Query => render_query(frame, layout[1], app),
        Tab::Analytics => render_analytics(frame, layout[1], app),
    }
    render_status(frame, layout[2], app);
    render_command(frame, layout[3], app, embedded);
    match app.mode {
        Mode::Create => render_form(frame, area, app, false),
        Mode::Edit => render_form(frame, area, app, true),
        Mode::Confirm => render_confirmation(frame, area, app),
        _ => {}
    }
}

fn render_tabs(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let titles = Tab::ALL
        .iter()
        .map(|tab| {
            let style = if *tab == app.tab {
                Style::default()
                    .fg(BG)
                    .bg(GREEN)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(DIM_GREEN).bg(BG)
            };
            Line::from(Span::styled(format!(" {} ", tab.title()), style))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Tabs::new(titles)
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(DIM_GREEN))
                    .bg(BG),
            )
            .bg(BG),
        Rect { height: 2, ..area },
    );
    let hint = match app.tab {
        Tab::Graph => "drag move  Shift+drag lasso  z fit  +/- zoom  a layout  p pin  o open code",
        Tab::Explorer => "j/k select  Space mark  c create  e edit  d delete  l link  o open code",
        Tab::Query => "Tab/←/→ focus  Enter edit  F5 run  F6 confirm write  PgUp/PgDn results",
        Tab::Analytics => "health metrics  :repair  :export/:import  U undo  R redo  r refresh",
    };
    frame.render_widget(
        Paragraph::new(Span::styled(hint, Style::default().fg(DARK_GRAY))).bg(BG),
        Rect {
            y: area.y + 2,
            height: 1,
            ..area
        },
    );
}

fn graph_panels(area: Rect) -> (Rect, Rect) {
    let inspector_width = (area.width / 3).clamp(30, 44);
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(30), Constraint::Length(inspector_width)])
        .split(area);
    (panels[0], panels[1])
}

fn graph_canvas(graph_area: Rect) -> Rect {
    Rect {
        x: graph_area.x.saturating_add(1),
        y: graph_area.y.saturating_add(1),
        width: graph_area.width.saturating_sub(3),
        height: graph_area.height.saturating_sub(2),
    }
}

fn screen_position(app: &App, fact: &Fact, canvas: Rect) -> Option<(u16, u16)> {
    let view_x = (fact.graph_x - app.center_x) * app.zoom + 0.5;
    let view_y = (fact.graph_y - app.center_y) * app.zoom + 0.5;
    if !(0.0..=1.0).contains(&view_x) || !(0.0..=1.0).contains(&view_y) {
        return None;
    }
    Some((
        canvas.x + (view_x * f64::from(canvas.width.saturating_sub(1))).round() as u16,
        canvas.y + (view_y * f64::from(canvas.height.saturating_sub(1))).round() as u16,
    ))
}

fn viewport_indices(app: &App) -> Vec<usize> {
    let filter = GraphFilter::from_labels(&app.dataset_filter, &app.type_filter);
    app.spatial_index
        .query_viewport(
            Viewport {
                center_x: app.center_x,
                center_y: app.center_y,
                zoom: app.zoom,
            },
            0.03 / app.zoom.max(0.5),
        )
        .into_iter()
        .filter(|index| {
            app.facts
                .get(*index)
                .is_some_and(|fact| filter.matches(fact))
        })
        .collect()
}

fn draw_edge(buffer: &mut Buffer, start: (u16, u16), end: (u16, u16), style: Style) {
    let (mut x, mut y) = (i32::from(start.0), i32::from(start.1));
    let (end_x, end_y) = (i32::from(end.0), i32::from(end.1));
    let dx = (end_x - x).abs();
    let sx = if x < end_x { 1 } else { -1 };
    let dy = -(end_y - y).abs();
    let sy = if y < end_y { 1 } else { -1 };
    let mut error = dx + dy;
    let mut step = 0usize;
    loop {
        if step.is_multiple_of(2) {
            if let Some(cell) = buffer.cell_mut((x as u16, y as u16)) {
                cell.set_symbol("·").set_style(style);
            }
        }
        if x == end_x && y == end_y {
            break;
        }
        let error2 = 2 * error;
        if error2 >= dy {
            error += dy;
            x += sx;
        }
        if error2 <= dx {
            error += dx;
            y += sy;
        }
        step += 1;
    }
    if let Some(cell) = buffer.cell_mut(end) {
        cell.set_symbol("◆").set_style(style);
    }
}

fn draw_lasso(buffer: &mut Buffer, start: (u16, u16), end: (u16, u16), canvas: Rect) {
    if canvas.width == 0 || canvas.height == 0 {
        return;
    }
    let left = start
        .0
        .min(end.0)
        .clamp(canvas.x, canvas.right().saturating_sub(1));
    let right = start
        .0
        .max(end.0)
        .clamp(canvas.x, canvas.right().saturating_sub(1));
    let top = start
        .1
        .min(end.1)
        .clamp(canvas.y, canvas.bottom().saturating_sub(1));
    let bottom = start
        .1
        .max(end.1)
        .clamp(canvas.y, canvas.bottom().saturating_sub(1));
    let style = Style::default().fg(CYAN).add_modifier(Modifier::BOLD);
    for x in left..=right {
        if let Some(cell) = buffer.cell_mut((x, top)) {
            cell.set_symbol("─").set_style(style);
        }
        if let Some(cell) = buffer.cell_mut((x, bottom)) {
            cell.set_symbol("─").set_style(style);
        }
    }
    for y in top..=bottom {
        if let Some(cell) = buffer.cell_mut((left, y)) {
            cell.set_symbol("│").set_style(style);
        }
        if let Some(cell) = buffer.cell_mut((right, y)) {
            cell.set_symbol("│").set_style(style);
        }
    }
    for (point, symbol) in [
        ((left, top), "┌"),
        ((right, top), "┐"),
        ((left, bottom), "└"),
        ((right, bottom), "┘"),
    ] {
        if let Some(cell) = buffer.cell_mut(point) {
            cell.set_symbol(symbol).set_style(style);
        }
    }
}

fn render_graph(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let (graph_area, inspector) = graph_panels(area);
    let canvas = graph_canvas(graph_area);
    let visible = visible_indices(app);
    let visible_ids: BTreeSet<&str> = visible
        .iter()
        .map(|index| app.facts[*index].id.as_str())
        .collect();
    let visible_edges = app
        .edges
        .iter()
        .filter(|edge| {
            visible_ids.contains(edge.from_id.as_str()) && visible_ids.contains(edge.to_id.as_str())
        })
        .count();
    let canvas_indices = viewport_indices(app);
    let canvas_ids = canvas_indices
        .iter()
        .map(|index| app.facts[*index].id.as_str())
        .collect::<BTreeSet<_>>();
    let block = Block::default()
        .borders(Borders::RIGHT)
        .title(Span::styled(
            format!(
                " Knowledge Graph · {} / {} · {} shown · {} loaded/{} total · {} edges · {:.1}x ",
                app.dataset_filter,
                app.type_filter,
                canvas_indices.len(),
                app.facts.len(),
                app.total_facts,
                visible_edges,
                app.zoom
            ),
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(DIM_GREEN))
        .bg(BG);
    frame.render_widget(block, graph_area);

    for y in (canvas.y..canvas.bottom()).step_by(4) {
        for x in (canvas.x..canvas.right()).step_by(8) {
            if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
                cell.set_symbol("·")
                    .set_style(Style::default().fg(DARK_GRAY));
            }
        }
    }

    let selected_edge = connected_edges(app).get(app.edge_selected).copied();
    for (index, edge) in app.edges.iter().enumerate() {
        if !canvas_ids.contains(edge.from_id.as_str()) || !canvas_ids.contains(edge.to_id.as_str())
        {
            continue;
        }
        let Some(from) = app
            .fact_lookup
            .get(&edge.from_id)
            .and_then(|index| app.facts.get(*index))
        else {
            continue;
        };
        let Some(to) = app
            .fact_lookup
            .get(&edge.to_id)
            .and_then(|index| app.facts.get(*index))
        else {
            continue;
        };
        let (Some(start), Some(end)) = (
            screen_position(app, from, canvas),
            screen_position(app, to, canvas),
        ) else {
            continue;
        };
        let is_selected = selected_edge == Some(index);
        let style = if is_selected {
            Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)
        } else if edge.relation_type == "proves" {
            Style::default().fg(MAGENTA)
        } else {
            Style::default().fg(DIM_GREEN)
        };
        draw_edge(frame.buffer_mut(), start, end, style);
        if is_selected {
            let middle = ((start.0 + end.0) / 2, (start.1 + end.1) / 2);
            frame.buffer_mut().set_stringn(
                middle.0,
                middle.1,
                format!(" {} ", edge.relation_type),
                canvas.right().saturating_sub(middle.0) as usize,
                Style::default().fg(BG).bg(YELLOW),
            );
        }
    }

    for selected_pass in [false, true] {
        for index in &canvas_indices {
            let fact = &app.facts[*index];
            let is_selected = *index == app.selected;
            if is_selected != selected_pass {
                continue;
            }
            let Some((x, y)) = screen_position(app, fact, canvas) else {
                continue;
            };
            let marked = app.marked.contains(&fact.id);
            let link_source = app.link_source.as_deref() == Some(fact.id.as_str());
            let style = if is_selected {
                Style::default()
                    .fg(BG)
                    .bg(GREEN)
                    .add_modifier(Modifier::BOLD)
            } else if link_source {
                Style::default()
                    .fg(BG)
                    .bg(YELLOW)
                    .add_modifier(Modifier::BOLD)
            } else if marked {
                Style::default().fg(BG).bg(CYAN)
            } else {
                Style::default().fg(type_color(&fact.fact_type)).bg(BG)
            };
            let symbol = if fact.graph_pinned { "◆" } else { "●" };
            frame.buffer_mut().set_stringn(
                x,
                y,
                format!("{symbol} {}", trunc(&fact.content.replace('\n', " "), 18)),
                canvas.right().saturating_sub(x) as usize,
                style,
            );
        }
    }

    if let (Some(start), Some(end)) = (app.lasso_start, app.lasso_end) {
        draw_lasso(frame.buffer_mut(), start, end, canvas);
    }

    if visible.is_empty() {
        frame.render_widget(
            Paragraph::new(if app.facts.is_empty() {
                "No knowledge units. Press c to create one."
            } else {
                "No nodes match the active filters."
            })
            .alignment(Alignment::Center)
            .style(Style::default().fg(GRAY).bg(BG)),
            Rect {
                y: canvas.y + canvas.height / 2,
                height: 1,
                ..canvas
            },
        );
    }
    render_inspector(frame, inspector, app);
}

fn render_inspector(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let (details_area, minimap_area) = if app.tab == Tab::Graph && area.height >= 27 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(12), Constraint::Length(9)])
            .split(area);
        (rows[0], Some(rows[1]))
    } else {
        (area, None)
    };
    let mut lines = vec![
        Line::from(Span::styled(
            " GRAPH INSPECTOR",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled(" filter ", Style::default().fg(GRAY)),
            Span::styled(&app.dataset_filter, Style::default().fg(YELLOW)),
            Span::styled(" / ", Style::default().fg(DARK_GRAY)),
            Span::styled(&app.type_filter, Style::default().fg(CYAN)),
        ]),
        Line::from(Span::styled(
            format!(
                " {} total · {} marked · {} edges",
                app.facts.len(),
                app.marked.len(),
                app.edges.len()
            ),
            Style::default().fg(DIM_GREEN),
        )),
        Line::from(""),
    ];
    if let Some(fact) = selected_fact(app) {
        lines.extend([
            Line::from(Span::styled(
                format!(
                    " {} {}",
                    if fact.graph_pinned { "◆" } else { "●" },
                    fact.fact_type
                ),
                Style::default()
                    .fg(type_color(&fact.fact_type))
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!(" {}", fact.id),
                Style::default().fg(DIM_GREEN),
            )),
            Line::from(""),
            Line::from(Span::styled(
                format!(" {}", fact.content),
                Style::default().fg(WHITE),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(" dataset    ", Style::default().fg(GRAY)),
                Span::styled(&fact.dataset, Style::default().fg(YELLOW)),
            ]),
            Line::from(vec![
                Span::styled(" source     ", Style::default().fg(GRAY)),
                Span::styled(&fact.source, Style::default().fg(DIM_GREEN)),
            ]),
            Line::from(vec![
                Span::styled(" confidence ", Style::default().fg(GRAY)),
                Span::styled(
                    format!("{:.2}", fact.confidence),
                    Style::default().fg(MAGENTA),
                ),
            ]),
            Line::from(vec![
                Span::styled(" position   ", Style::default().fg(GRAY)),
                Span::styled(
                    format!("{:.3}, {:.3}", fact.graph_x, fact.graph_y),
                    Style::default().fg(CYAN),
                ),
            ]),
            Line::from(vec![
                Span::styled(" tags       ", Style::default().fg(GRAY)),
                Span::styled(
                    if fact.tags.is_empty() {
                        "-".into()
                    } else {
                        fact.tags.join(", ")
                    },
                    Style::default().fg(DIM_GREEN),
                ),
            ]),
            Line::from(vec![
                Span::styled(" code       ", Style::default().fg(GRAY)),
                Span::styled(
                    fact.code_path
                        .as_ref()
                        .map(|path| {
                            format!(
                                "{}:{}-{}",
                                path,
                                fact.code_start_line.unwrap_or(1),
                                fact.code_end_line
                                    .unwrap_or(fact.code_start_line.unwrap_or(1))
                            )
                        })
                        .unwrap_or_else(|| "-".into()),
                    Style::default().fg(CYAN),
                ),
            ]),
            Line::from(vec![
                Span::styled(" run        ", Style::default().fg(GRAY)),
                Span::styled(
                    fact.run_id.as_deref().unwrap_or("-"),
                    Style::default().fg(MAGENTA),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                " RELATIONS",
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            )),
        ]);
        let connected = connected_edges(app);
        if connected.is_empty() {
            lines.push(Line::from(Span::styled(
                " No connected edges",
                Style::default().fg(GRAY),
            )));
        }
        for (position, edge_index) in connected.iter().enumerate() {
            let edge = &app.edges[*edge_index];
            let outgoing = edge.from_id == fact.id;
            let other_id = if outgoing { &edge.to_id } else { &edge.from_id };
            let other = app
                .fact_lookup
                .get(other_id)
                .and_then(|index| app.facts.get(*index))
                .map(|candidate| trunc(&candidate.content, 18))
                .unwrap_or_else(|| other_id.clone());
            lines.push(Line::from(Span::styled(
                format!(
                    " {} {} {} · {other}",
                    if position == app.edge_selected {
                        "▸"
                    } else {
                        " "
                    },
                    if outgoing { "→" } else { "←" },
                    edge.relation_type
                ),
                if position == app.edge_selected {
                    Style::default().fg(YELLOW)
                } else {
                    Style::default().fg(DIM_GREEN)
                },
            )));
        }
    } else {
        lines.push(Line::from(Span::styled(
            " No visible node selected",
            Style::default().fg(GRAY),
        )));
    }
    if let Some(source) = &app.link_source {
        lines.extend([
            Line::from(""),
            Line::from(Span::styled(
                format!(" LINK SOURCE {source}"),
                Style::default().fg(BG).bg(YELLOW),
            )),
            Line::from(Span::styled(
                format!(" Select target; l creates {}", app.relation_type),
                Style::default().fg(YELLOW),
            )),
        ]);
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .bg(BG)
            .wrap(Wrap { trim: false }),
        details_area,
    );
    if let Some(area) = minimap_area {
        render_minimap(frame, area, app);
    }
}

fn minimap_position(fact: &Fact, area: Rect) -> (u16, u16) {
    (
        area.x
            + (fact.graph_x.clamp(0.0, 1.0) * f64::from(area.width.saturating_sub(1))).round()
                as u16,
        area.y
            + (fact.graph_y.clamp(0.0, 1.0) * f64::from(area.height.saturating_sub(1))).round()
                as u16,
    )
}

fn minimap_viewport_position(x: f64, y: f64, area: Rect) -> (u16, u16) {
    (
        area.x + (x.clamp(0.0, 1.0) * f64::from(area.width.saturating_sub(1))).round() as u16,
        area.y + (y.clamp(0.0, 1.0) * f64::from(area.height.saturating_sub(1))).round() as u16,
    )
}

fn render_minimap(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Minimap · {} ", app.facts.len()))
            .border_style(Style::default().fg(DIM_GREEN))
            .bg(BG),
        area,
    );
    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    for fact in &app.facts {
        let point = minimap_position(fact, inner);
        let style = if fact_visible(app, fact) {
            Style::default().fg(type_color(&fact.fact_type))
        } else {
            Style::default().fg(DARK_GRAY)
        };
        if let Some(cell) = frame.buffer_mut().cell_mut(point) {
            cell.set_symbol("·").set_style(style);
        }
    }
    let bounds = Viewport {
        center_x: app.center_x,
        center_y: app.center_y,
        zoom: app.zoom,
    }
    .world_bounds(0.0);
    draw_lasso(
        frame.buffer_mut(),
        minimap_viewport_position(bounds.0, bounds.1, inner),
        minimap_viewport_position(bounds.2, bounds.3, inner),
        inner,
    );
    if let Some(fact) = selected_fact(app) {
        if let Some(cell) = frame.buffer_mut().cell_mut(minimap_position(fact, inner)) {
            cell.set_symbol("◆")
                .set_style(Style::default().fg(YELLOW).add_modifier(Modifier::BOLD));
        }
    }
}

fn render_explorer(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(23),
            Constraint::Percentage(36),
            Constraint::Percentage(41),
        ])
        .split(area);
    render_filter_tree(frame, panels[0], app);
    render_node_list(frame, panels[1], app);
    render_inspector(frame, panels[2], app);
}

fn render_filter_tree(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let mut lines = vec![Line::from(Span::styled(
        " DATASETS",
        Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
    ))];
    let all_style = if app.dataset_filter == "all" {
        Style::default().fg(BG).bg(YELLOW)
    } else {
        Style::default().fg(DIM_GREEN)
    };
    lines.push(Line::from(Span::styled(
        format!(" all ({}) ", app.facts.len()),
        all_style,
    )));
    for dataset in &app.datasets {
        let count = app
            .facts
            .iter()
            .filter(|fact| fact.dataset == dataset.name)
            .count();
        let style = if app.dataset_filter == dataset.name {
            Style::default().fg(BG).bg(YELLOW)
        } else {
            Style::default().fg(DIM_GREEN)
        };
        lines.push(Line::from(Span::styled(
            format!(" {} ({count}) ", dataset.name),
            style,
        )));
    }
    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            " TYPES",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )),
    ]);
    let mut counts = BTreeMap::<&str, usize>::new();
    for fact in &app.facts {
        *counts.entry(&fact.fact_type).or_default() += 1;
    }
    lines.push(Line::from(Span::styled(
        format!(" all ({}) ", app.facts.len()),
        if app.type_filter == "all" {
            Style::default().fg(BG).bg(CYAN)
        } else {
            Style::default().fg(DIM_GREEN)
        },
    )));
    for (fact_type, count) in counts {
        lines.push(Line::from(Span::styled(
            format!(" {fact_type} ({count}) "),
            if app.type_filter == fact_type {
                Style::default().fg(BG).bg(CYAN)
            } else {
                Style::default().fg(type_color(fact_type))
            },
        )));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .borders(Borders::RIGHT)
                    .border_style(Style::default().fg(DIM_GREEN))
                    .bg(BG),
            )
            .bg(BG),
        area,
    );
}

fn virtual_window(len: usize, selected: usize, capacity: usize) -> (usize, usize) {
    let capacity = capacity.max(1);
    let start = selected
        .min(len.saturating_sub(1))
        .saturating_sub(capacity / 2)
        .min(len.saturating_sub(capacity));
    (start, (start + capacity).min(len))
}

fn render_node_list(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let visible = visible_indices(app);
    let selected_position = visible.iter().position(|index| *index == app.selected);
    let capacity = usize::from(area.height.saturating_sub(2).max(1));
    let (start, end) = virtual_window(visible.len(), selected_position.unwrap_or(0), capacity);
    let items = visible[start..end]
        .iter()
        .map(|index| {
            let fact = &app.facts[*index];
            let selected = *index == app.selected;
            let marked = app.marked.contains(&fact.id);
            let style = if selected {
                Style::default().fg(BG).bg(GREEN)
            } else if marked {
                Style::default().fg(BG).bg(CYAN)
            } else {
                Style::default().fg(type_color(&fact.fact_type)).bg(BG)
            };
            ListItem::new(Line::from(Span::styled(
                format!(
                    "{} {} [{}] {}",
                    if marked { "■" } else { "□" },
                    if fact.graph_pinned { "◆" } else { "●" },
                    fact.fact_type,
                    trunc(&fact.content.replace('\n', " "), 42)
                ),
                style,
            )))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    state.select(selected_position.map(|position| position.saturating_sub(start)));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(format!(
                        " Knowledge Units · {}-{} / {} · {} total ",
                        if visible.is_empty() { 0 } else { start + 1 },
                        end,
                        visible.len(),
                        app.total_facts
                    ))
                    .borders(Borders::RIGHT)
                    .border_style(Style::default().fg(DIM_GREEN))
                    .bg(BG),
            )
            .bg(BG),
        area,
        &mut state,
    );
}

const QUERY_TEMPLATES: [(&str, &str); 6] = [
    ("Recent facts", "SELECT * FROM fact ORDER BY timestamp DESC LIMIT 25"),
    (
        "Low confidence",
        "SELECT id, fact_type, content, confidence FROM fact WHERE confidence < 0.5 ORDER BY confidence",
    ),
    (
        "Orphan candidates",
        "SELECT id, fact_type, content FROM fact WHERE count(<-relates_to)+count(->relates_to)+count(<-proves)+count(->proves) = 0",
    ),
    ("Datasets", "SELECT * FROM dataset ORDER BY name"),
    ("Relation counts", "SELECT count() FROM relates_to GROUP ALL; SELECT count() FROM proves GROUP ALL"),
    ("Database schema", "INFO FOR DB"),
];

#[derive(Clone, Copy)]
struct QueryLayout {
    templates: Rect,
    history: Rect,
    editor: Rect,
    results: Rect,
}

fn query_layout(area: Rect) -> QueryLayout {
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(32), Constraint::Min(24)])
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length((QUERY_TEMPLATES.len() as u16).saturating_add(2)),
            Constraint::Min(3),
        ])
        .split(panels[0]);
    let editor_height = (area.height / 3).clamp(5, 12);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(editor_height), Constraint::Min(3)])
        .split(panels[1]);
    QueryLayout {
        templates: left[0],
        history: left[1],
        editor: right[0],
        results: right[1],
    }
}

fn query_border(app: &App, focus: QueryFocus) -> Style {
    if app.query_focus == focus {
        Style::default().fg(GREEN)
    } else {
        Style::default().fg(DIM_GREEN)
    }
}

fn query_history_offset(app: &App, height: usize) -> usize {
    if height == 0 || app.query_history.len() <= height {
        return 0;
    }
    app.history_selected
        .saturating_sub(height / 2)
        .min(app.query_history.len() - height)
}

fn render_query(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let layout = query_layout(area);
    let templates = QUERY_TEMPLATES
        .iter()
        .enumerate()
        .map(|(index, (name, _))| {
            let selected = index == app.template_selected;
            ListItem::new(Line::from(Span::styled(
                format!(" {} {name}", if selected { "▸" } else { " " }),
                if selected {
                    Style::default().fg(BG).bg(YELLOW)
                } else {
                    Style::default().fg(DIM_GREEN).bg(BG)
                },
            )))
        })
        .collect::<Vec<_>>();
    let mut template_state = ListState::default();
    template_state.select(Some(app.template_selected));
    frame.render_stateful_widget(
        List::new(templates)
            .block(
                Block::default()
                    .title(" Templates ")
                    .borders(Borders::ALL)
                    .border_style(query_border(app, QueryFocus::Templates))
                    .bg(BG),
            )
            .bg(BG),
        layout.templates,
        &mut template_state,
    );

    let history_height = layout.history.height.saturating_sub(2) as usize;
    let history_offset = query_history_offset(app, history_height);
    let history = app
        .query_history
        .iter()
        .enumerate()
        .skip(history_offset)
        .take(history_height)
        .map(|(index, query)| {
            let selected = index == app.history_selected;
            let preview = query.replace('\n', " ");
            ListItem::new(Line::from(Span::styled(
                format!(
                    " {} {}",
                    if selected { "▸" } else { " " },
                    trunc(&preview, 25)
                ),
                if selected {
                    Style::default().fg(BG).bg(CYAN)
                } else {
                    Style::default().fg(GRAY).bg(BG)
                },
            )))
        })
        .collect::<Vec<_>>();
    let mut history_state = ListState::default();
    history_state.select(
        (!app.query_history.is_empty())
            .then_some(app.history_selected.saturating_sub(history_offset)),
    );
    frame.render_stateful_widget(
        List::new(history)
            .block(
                Block::default()
                    .title(" History ")
                    .borders(Borders::ALL)
                    .border_style(query_border(app, QueryFocus::History))
                    .bg(BG),
            )
            .bg(BG),
        layout.history,
        &mut history_state,
    );

    frame.render_widget(
        Block::default()
            .title(" SurrealQL · F5 run · F6 confirm write ")
            .borders(Borders::ALL)
            .border_style(query_border(app, QueryFocus::Editor))
            .bg(BG),
        layout.editor,
    );
    frame.render_widget(
        &app.query_editor,
        layout.editor.inner(Margin {
            horizontal: 1,
            vertical: 1,
        }),
    );

    frame.render_widget(
        Paragraph::new(if app.query_output.is_empty() {
            "No result yet."
        } else {
            app.query_output.as_str()
        })
        .block(
            Block::default()
                .title(" Results ")
                .borders(Borders::ALL)
                .border_style(query_border(app, QueryFocus::Results))
                .bg(BG),
        )
        .scroll((app.query_scroll.min(u16::MAX as usize) as u16, 0))
        .wrap(Wrap { trim: false })
        .bg(BG),
        layout.results,
    );
}

fn graph_metrics(app: &App) -> (HashMap<String, usize>, usize, usize, usize) {
    let analytics = if app.analytics.node_count == app.facts.len()
        && app.analytics.edge_count == app.edges.len()
    {
        app.analytics.clone()
    } else {
        GraphAnalytics::compute(&app.facts, &app.edges)
    };
    (
        analytics.degrees,
        analytics.orphan_count,
        analytics.duplicate_edge_count,
        analytics.pinned_count,
    )
}

fn metric_bar(label: &str, count: usize, max: usize, color: Color) -> Line<'static> {
    let width = count.saturating_mul(24).checked_div(max).unwrap_or(0);
    Line::from(vec![
        Span::styled(format!(" {label:<18}"), Style::default().fg(GRAY)),
        Span::styled("█".repeat(width), Style::default().fg(color)),
        Span::styled(format!(" {count}"), Style::default().fg(WHITE)),
    ])
}

fn render_analytics(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let (degrees, orphans, duplicates, pinned) = graph_metrics(app);
    let average_degree = app.analytics.average_degree;
    let max_degree = degrees.values().copied().max().unwrap_or(0);
    let mut left = vec![
        Line::from(Span::styled(
            " GRAPH HEALTH",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(" nodes          ", Style::default().fg(GRAY)),
            Span::styled(app.facts.len().to_string(), Style::default().fg(GREEN)),
        ]),
        Line::from(vec![
            Span::styled(" edges          ", Style::default().fg(GRAY)),
            Span::styled(app.edges.len().to_string(), Style::default().fg(MAGENTA)),
        ]),
        Line::from(vec![
            Span::styled(" datasets       ", Style::default().fg(GRAY)),
            Span::styled(app.datasets.len().to_string(), Style::default().fg(YELLOW)),
        ]),
        Line::from(vec![
            Span::styled(" average degree ", Style::default().fg(GRAY)),
            Span::styled(format!("{average_degree:.2}"), Style::default().fg(CYAN)),
        ]),
        Line::from(vec![
            Span::styled(" maximum degree ", Style::default().fg(GRAY)),
            Span::styled(max_degree.to_string(), Style::default().fg(CYAN)),
        ]),
        Line::from(vec![
            Span::styled(" orphan nodes   ", Style::default().fg(GRAY)),
            Span::styled(
                orphans.to_string(),
                Style::default().fg(if orphans == 0 { GREEN } else { YELLOW }),
            ),
        ]),
        Line::from(vec![
            Span::styled(" duplicate edges", Style::default().fg(GRAY)),
            Span::styled(
                format!(" {duplicates}"),
                Style::default().fg(if duplicates == 0 { GREEN } else { RED }),
            ),
        ]),
        Line::from(vec![
            Span::styled(" pinned nodes   ", Style::default().fg(GRAY)),
            Span::styled(pinned.to_string(), Style::default().fg(YELLOW)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            if duplicates == 0 {
                " Integrity: healthy"
            } else {
                " Integrity: duplicate relations detected"
            },
            Style::default().fg(if duplicates == 0 { GREEN } else { RED }),
        )),
        Line::from(""),
        Line::from(Span::styled(
            " :repair  :export [path]  U=undo",
            Style::default().fg(DARK_GRAY),
        )),
    ];
    let type_counts = app.analytics.type_counts.clone();
    let dataset_counts = app.analytics.dataset_counts.clone();
    let max_count = type_counts
        .values()
        .chain(dataset_counts.values())
        .copied()
        .max()
        .unwrap_or(1);
    let mut right = vec![Line::from(Span::styled(
        " DISTRIBUTION",
        Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
    ))];
    for (name, count) in type_counts {
        right.push(metric_bar(&name, count, max_count, type_color(&name)));
    }
    right.extend([
        Line::from(""),
        Line::from(Span::styled(" DATASETS", Style::default().fg(YELLOW))),
    ]);
    for (name, count) in dataset_counts {
        right.push(metric_bar(&name, count, max_count, YELLOW));
    }
    left.shrink_to_fit();
    frame.render_widget(
        Paragraph::new(Text::from(left))
            .block(
                Block::default()
                    .borders(Borders::RIGHT)
                    .border_style(Style::default().fg(DIM_GREEN))
                    .bg(BG),
            )
            .bg(BG),
        columns[0],
    );
    frame.render_widget(Paragraph::new(Text::from(right)).bg(BG), columns[1]);
}

fn render_status(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let mode = match app.mode {
        Mode::Normal => "NORMAL",
        Mode::Command => "COMMAND",
        Mode::Search => "SEARCH",
        Mode::QueryEdit => "QUERY EDIT",
        Mode::Create => "CREATE",
        Mode::Edit => "EDIT",
        Mode::Confirm => "CONFIRM",
    };
    let operation = app.active_job.as_ref().map(|active| {
        let elapsed = active.started.elapsed().as_secs_f32();
        let spinner = ["·", "o", "O", "o"][((elapsed * 8.0) as usize) % 4];
        format!(
            "{spinner} {} {}% {} {:.1}s",
            active.kind.label(),
            active.percent,
            active.phase,
            elapsed
        )
    });
    let text = if let Some(operation) = operation {
        format!(" {} · {mode} · {operation} · Esc cancel", app.tab.title())
    } else {
        format!(
            " {} · {mode} · {}/{} nodes · {} edges · {}",
            app.tab.title(),
            app.facts.len(),
            app.total_facts,
            app.edges.len(),
            app.status
        )
    };
    frame.render_widget(
        Paragraph::new(Span::styled(text, Style::default().fg(BG).bg(DIM_GREEN))),
        area,
    );
}

fn render_command(frame: &mut ratatui::Frame, area: Rect, app: &App, embedded: bool) {
    let line = match app.mode {
        Mode::Command => Line::from(vec![
            Span::styled(":", Style::default().fg(YELLOW)),
            Span::styled(&app.command, Style::default().fg(WHITE)),
            Span::styled("█", Style::default().fg(GREEN)),
        ]),
        Mode::Search => Line::from(vec![
            Span::styled("/", Style::default().fg(YELLOW)),
            Span::styled(&app.command, Style::default().fg(WHITE)),
            Span::styled("█", Style::default().fg(GREEN)),
        ]),
        Mode::QueryEdit => Line::from(Span::styled(
            "Esc focus navigation · F5/Ctrl+Enter run · F6 confirm mutation",
            Style::default().fg(CYAN),
        )),
        _ if app.active_job.is_some() => Line::from(Span::styled(
            "Esc cancel operation  :retry reruns last safe operation",
            Style::default().fg(YELLOW),
        )),
        _ => Line::from(Span::styled(
            format!(
                ":help  1/2/3/4 views  Space mark  U undo  R redo  r refresh  q {}",
                if embedded { "return" } else { "quit" }
            ),
            Style::default().fg(DARK_GRAY),
        )),
    };
    frame.render_widget(Paragraph::new(line).bg(BG), area);
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

fn render_form(frame: &mut ratatui::Frame, root: Rect, app: &App, editing: bool) {
    let area = centered_rect(root, 72, 15);
    frame.render_widget(Clear, area);
    let labels = ["Type", "Content", "Dataset", "Tags", "Confidence"];
    let mut lines = vec![Line::from(Span::styled(
        if editing {
            " EDIT KNOWLEDGE UNIT"
        } else {
            " CREATE KNOWLEDGE UNIT"
        },
        Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
    ))];
    for (index, label) in labels.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(format!(" {label:<12}"), Style::default().fg(GRAY)),
            Span::styled(
                if index == 1 {
                    trunc(&app.form_values[index].replace('\n', " "), 52)
                } else {
                    app.form_values[index].clone()
                },
                if index == app.form_field {
                    Style::default().fg(BG).bg(GREEN)
                } else {
                    Style::default().fg(WHITE)
                },
            ),
            if index == app.form_field {
                Span::styled("█", Style::default().fg(GREEN))
            } else {
                Span::raw("")
            },
        ]));
    }
    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            " Tab/Shift+Tab field · Enter save · Esc cancel",
            Style::default().fg(DARK_GRAY),
        )),
    ]);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(GREEN))
                    .bg(BG),
            )
            .bg(BG),
        area,
    );
}

fn confirmation_text(action: &ConfirmAction) -> String {
    match action {
        ConfirmAction::Facts(ids) => format!(
            "Delete {} knowledge unit(s) and their connected edges?",
            ids.len()
        ),
        ConfirmAction::Edge(id) => format!("Delete relation {id}?"),
        ConfirmAction::Dataset(name) => {
            format!("Delete dataset {name}? Its units will be moved to default.")
        }
        ConfirmAction::Query(sql) => format!(
            "Execute mutating SurrealQL: {}",
            trunc(&sql.replace('\n', " "), 42)
        ),
    }
}

fn render_confirmation(frame: &mut ratatui::Frame, root: Rect, app: &App) {
    let Some(action) = &app.confirm else {
        return;
    };
    let area = centered_rect(root, 68, 7);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                " CONFIRM DATABASE CHANGE",
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                format!(" {}", confirmation_text(action)),
                Style::default().fg(WHITE),
            )),
            Line::from(Span::styled(
                if matches!(action, ConfirmAction::Query(_)) {
                    " y confirm · n/Esc cancel · arbitrary writes may not be undoable"
                } else {
                    " y confirm · n/Esc cancel · action remains undoable"
                },
                Style::default().fg(YELLOW),
            )),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(RED))
                .bg(BG),
        )
        .bg(BG),
        area,
    );
}

async fn persist_position(fact: &Fact) -> Result<(), String> {
    GraphRepository::persist_position(fact).await
}

async fn move_selected(app: &mut App, dx: f64, dy: f64) {
    if mutation_blocked(app) {
        return;
    }
    if selected_fact(app).is_none() {
        app.status = "no visible node selected".into();
        return;
    }
    let fact = &mut app.facts[app.selected];
    fact.graph_x = (fact.graph_x + dx / app.zoom).clamp(0.0, 1.0);
    fact.graph_y = (fact.graph_y + dy / app.zoom).clamp(0.0, 1.0);
    let changed = fact.clone();
    match persist_position(&changed).await {
        Ok(()) => {
            rebuild_graph_caches(app);
            app.status = format!(
                "moved {} to {:.3}, {:.3}",
                changed.id, changed.graph_x, changed.graph_y
            )
        }
        Err(error) => app.status = format!("move failed: {error}"),
    }
}

async fn toggle_pin(app: &mut App) {
    if mutation_blocked(app) {
        return;
    }
    if selected_fact(app).is_none() {
        app.status = "no visible node selected".into();
        return;
    }
    app.facts[app.selected].graph_pinned = !app.facts[app.selected].graph_pinned;
    let changed = app.facts[app.selected].clone();
    match persist_position(&changed).await {
        Ok(()) => {
            rebuild_graph_caches(app);
            app.status = format!(
                "{} {}",
                if changed.graph_pinned {
                    "pinned"
                } else {
                    "unpinned"
                },
                changed.id
            )
        }
        Err(error) => app.status = format!("pin failed: {error}"),
    }
}

async fn create_relation(app: &mut App) {
    if mutation_blocked(app) {
        return;
    }
    let Some(target) = selected_fact(app).map(|fact| fact.id.clone()) else {
        app.status = "no visible target selected".into();
        return;
    };
    let Some(source) = app.link_source.take() else {
        app.link_source = Some(target.clone());
        app.status = format!(
            "link source {target}; select target and press l ({})",
            app.relation_type
        );
        return;
    };
    if source == target {
        app.status = "link cancelled: source equals target".into();
        return;
    }
    if app.edges.iter().any(|edge| {
        edge.relation_type == app.relation_type && edge.from_id == source && edge.to_id == target
    }) {
        app.status = "relation already exists".into();
        return;
    }
    match GraphRepository::relate(&source, &app.relation_type, &target).await {
        Ok(_) => match refresh(app).await {
            Ok(()) => app.status = format!("linked {source} -{}-> {target}", app.relation_type),
            Err(error) => app.status = format!("linked; refresh failed: {error}"),
        },
        Err(error) => app.status = format!("link failed: {error}"),
    }
}

fn cycle_edge(app: &mut App, offset: isize) {
    let count = connected_edges(app).len();
    if count == 0 {
        app.edge_selected = 0;
        return;
    }
    if offset.is_negative() {
        app.edge_selected = app.edge_selected.saturating_sub(offset.unsigned_abs());
    } else {
        app.edge_selected = (app.edge_selected + offset as usize).min(count - 1);
    }
}

fn auto_layout(app: &mut App) {
    start_graph_job(app, GraphJobKind::Layout);
}

fn open_create(app: &mut App) {
    if mutation_blocked(app) {
        return;
    }
    app.form_values = [
        "memory".into(),
        String::new(),
        if app.dataset_filter == "all" {
            "default".into()
        } else {
            app.dataset_filter.clone()
        },
        String::new(),
        "0.90".into(),
    ];
    app.form_field = 0;
    app.mode = Mode::Create;
}

fn open_edit(app: &mut App) {
    if mutation_blocked(app) {
        return;
    }
    let Some(fact) = selected_fact(app).cloned() else {
        app.status = "no visible node selected".into();
        return;
    };
    app.form_values = [
        fact.fact_type,
        fact.content,
        fact.dataset,
        fact.tags.join(","),
        format!("{:.2}", fact.confidence),
    ];
    app.form_field = 0;
    app.mode = Mode::Edit;
}

fn parse_form(app: &App) -> Result<(String, String, String, Vec<String>, f64), String> {
    let fact_type = app.form_values[0].trim().to_string();
    let content = app.form_values[1].trim().to_string();
    let dataset = app.form_values[2].trim().to_string();
    let tags = app.form_values[3]
        .split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(String::from)
        .collect::<Vec<_>>();
    let confidence = app.form_values[4]
        .trim()
        .parse::<f64>()
        .map_err(|_| "confidence must be a number from 0 to 1".to_string())?;
    if !valid_label(&fact_type) || !valid_label(&dataset) {
        return Err("type and dataset must be safe labels".into());
    }
    if content.is_empty() {
        return Err("content cannot be empty".into());
    }
    if !tags.iter().all(|tag| valid_label(tag)) {
        return Err("tags must be comma-separated safe labels".into());
    }
    if !(0.0..=1.0).contains(&confidence) {
        return Err("confidence must be from 0 to 1".into());
    }
    Ok((fact_type, content, dataset, tags, confidence))
}

fn sql_tags(tags: &[String]) -> String {
    format!(
        "[{}]",
        tags.iter()
            .map(|tag| sql_string(tag))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

async fn submit_form(app: &mut App, editing: bool) {
    let (fact_type, content, dataset, tags, confidence) = match parse_form(app) {
        Ok(values) => values,
        Err(error) => {
            app.status = error;
            return;
        }
    };
    let sql = if editing {
        let Some(fact) = selected_fact(app) else {
            app.status = "no visible node selected".into();
            return;
        };
        format!(
            "UPDATE {} SET fact_type = {}, content = {}, dataset = {}, tags = {}, confidence = {:.6}, updated_at = time::now()",
            fact.id,
            sql_string(&fact_type),
            sql_string(&content),
            sql_string(&dataset),
            sql_tags(&tags),
            confidence
        )
    } else {
        let angle = app.facts.len() as f64 * 2.399_963_229_728_653;
        format!(
            "CREATE fact CONTENT {{ fact_type: {}, content: {}, source: \"db_tui\", confidence: {:.6}, tags: {}, dataset: {}, graph_x: {:.6}, graph_y: {:.6}, graph_pinned: false, tool_origin: \"db_tui\", timestamp: time::now(), updated_at: time::now() }}",
            sql_string(&fact_type),
            sql_string(&content),
            confidence,
            sql_tags(&tags),
            sql_string(&dataset),
            (0.5 + angle.cos() * 0.25).clamp(0.05, 0.95),
            (0.5 + angle.sin() * 0.25).clamp(0.05, 0.95)
        )
    };
    match db(&sql).await {
        Ok(_) => {
            app.mode = Mode::Normal;
            match refresh(app).await {
                Ok(()) => {
                    if let Some(index) = app
                        .facts
                        .iter()
                        .position(|fact| fact.content == content && fact.dataset == dataset)
                    {
                        app.selected = index;
                    }
                    app.status = if editing {
                        "knowledge unit updated".into()
                    } else {
                        "knowledge unit created".into()
                    };
                }
                Err(error) => app.status = format!("saved; refresh failed: {error}"),
            }
        }
        Err(error) => app.status = format!("save failed: {error}"),
    }
}

fn audit_statement(action: &str, entity_id: &str, payload: &Value) -> String {
    format!(
        "CREATE graph_audit CONTENT {{ action: {}, entity_id: {}, payload: {}, timestamp: time::now(), undone: false }}",
        sql_string(action),
        sql_string(entity_id),
        sql_string(&payload.to_string())
    )
}

async fn execute_confirmation(app: &mut App) {
    let Some(action) = app.confirm.take() else {
        app.mode = Mode::Normal;
        return;
    };
    app.mode = Mode::Normal;
    match action {
        ConfirmAction::Facts(ids) => {
            let facts = app
                .facts
                .iter()
                .filter(|fact| ids.contains(&fact.id))
                .cloned()
                .collect::<Vec<_>>();
            let edges = app
                .edges
                .iter()
                .filter(|edge| ids.contains(&edge.from_id) || ids.contains(&edge.to_id))
                .cloned()
                .collect::<Vec<_>>();
            let payload = match serde_json::to_value(DeletedFacts {
                facts: facts.clone(),
                edges,
            }) {
                Ok(payload) => payload,
                Err(error) => {
                    app.status = format!("could not snapshot deletion: {error}");
                    return;
                }
            };
            let record_list = ids.join(", ");
            let deletes = ids
                .iter()
                .map(|id| format!("DELETE {id}"))
                .collect::<Vec<_>>()
                .join("; ");
            let sql = format!(
                "DELETE relates_to WHERE in IN [{record_list}] OR out IN [{record_list}]; DELETE proves WHERE in IN [{record_list}] OR out IN [{record_list}]; {deletes}; {}",
                audit_statement("delete_facts", &ids.join(","), &payload)
            );
            match db_transaction(&sql).await {
                Ok(_) => {
                    app.marked.clear();
                    match refresh(app).await {
                        Ok(()) => app.status = format!("deleted {} unit(s); U to undo", ids.len()),
                        Err(error) => app.status = format!("deleted; refresh failed: {error}"),
                    }
                }
                Err(error) => app.status = format!("delete failed: {error}"),
            }
        }
        ConfirmAction::Edge(id) => {
            let Some(edge) = app.edges.iter().find(|edge| edge.id == id).cloned() else {
                app.status = "relation disappeared before deletion".into();
                return;
            };
            let payload = match serde_json::to_value(&edge) {
                Ok(payload) => payload,
                Err(error) => {
                    app.status = format!("could not snapshot relation: {error}");
                    return;
                }
            };
            let sql = format!(
                "DELETE {id}; {}",
                audit_statement("delete_edge", &id, &payload)
            );
            match db_transaction(&sql).await {
                Ok(_) => match refresh(app).await {
                    Ok(()) => app.status = format!("deleted relation {id}; U to undo"),
                    Err(error) => app.status = format!("deleted; refresh failed: {error}"),
                },
                Err(error) => app.status = format!("unlink failed: {error}"),
            }
        }
        ConfirmAction::Dataset(name) => {
            if name == "default" {
                app.status = "the default dataset cannot be deleted".into();
                return;
            }
            let dataset = app
                .datasets
                .iter()
                .find(|dataset| dataset.name == name)
                .cloned()
                .unwrap_or(Dataset {
                    name: name.clone(),
                    description: String::new(),
                });
            let fact_ids = app
                .facts
                .iter()
                .filter(|fact| fact.dataset == name)
                .map(|fact| fact.id.clone())
                .collect::<Vec<_>>();
            let payload = json!({ "dataset": dataset, "fact_ids": fact_ids });
            let quoted = sql_string(&name);
            let sql = format!(
                "UPDATE fact SET dataset = \"default\", updated_at = time::now() WHERE dataset = {quoted}; DELETE dataset WHERE name = {quoted}; {}",
                audit_statement("delete_dataset", &name, &payload)
            );
            match db_transaction(&sql).await {
                Ok(_) => {
                    app.dataset_filter = "all".into();
                    match refresh(app).await {
                        Ok(()) => {
                            app.status = format!("deleted dataset {name}; units moved; U to undo")
                        }
                        Err(error) => app.status = format!("deleted; refresh failed: {error}"),
                    }
                }
                Err(error) => app.status = format!("dataset delete failed: {error}"),
            }
        }
        ConfirmAction::Query(sql) => queue_query(app, &sql, true),
    }
}

fn fact_restore_sql(fact: &Fact) -> String {
    format!(
        "CREATE {} CONTENT {{ fact_type: {}, content: {}, source: {}, confidence: {:.6}, tags: {}, dataset: {}, graph_x: {:.6}, graph_y: {:.6}, graph_pinned: {}, tool_origin: \"undo\", timestamp: time::now(), updated_at: time::now() }}",
        fact.id,
        sql_string(&fact.fact_type),
        sql_string(&fact.content),
        sql_string(&fact.source),
        fact.confidence,
        sql_tags(&fact.tags),
        sql_string(&fact.dataset),
        fact.graph_x,
        fact.graph_y,
        fact.graph_pinned
    )
}

async fn undo_last(app: &mut App) {
    let rows = match db("SELECT id, action, entity_id, payload, timestamp FROM graph_audit WHERE undone = false ORDER BY timestamp DESC LIMIT 1").await {
        Ok(rows) => rows,
        Err(error) => {
            app.status = format!("undo journal unavailable: {error}");
            return;
        }
    };
    let Some(row) = rows
        .first()
        .and_then(|row| row["result"].as_array())
        .and_then(|items| items.first())
    else {
        app.status = "nothing to undo".into();
        return;
    };
    let Some(audit_id) = row["id"].as_str() else {
        app.status = "invalid undo journal record".into();
        return;
    };
    let action = row["action"].as_str().unwrap_or_default();
    let payload: Value = match row["payload"]
        .as_str()
        .ok_or_else(|| "missing payload".to_string())
        .and_then(|payload| serde_json::from_str(payload).map_err(|error| error.to_string()))
    {
        Ok(payload) => payload,
        Err(error) => {
            app.status = format!("invalid undo payload: {error}");
            return;
        }
    };
    let restore_sql = match action {
        "delete_facts" => {
            let deleted: DeletedFacts = match serde_json::from_value(payload) {
                Ok(deleted) => deleted,
                Err(error) => {
                    app.status = format!("invalid fact snapshot: {error}");
                    return;
                }
            };
            let mut statements = deleted
                .facts
                .iter()
                .map(fact_restore_sql)
                .collect::<Vec<_>>();
            statements.extend(deleted.edges.iter().map(|edge| {
                format!(
                    "RELATE {}->{}->{}",
                    edge.from_id, edge.relation_type, edge.to_id
                )
            }));
            statements.join("; ")
        }
        "delete_edge" => {
            let edge: Edge = match serde_json::from_value(payload) {
                Ok(edge) => edge,
                Err(error) => {
                    app.status = format!("invalid edge snapshot: {error}");
                    return;
                }
            };
            format!(
                "RELATE {}->{}->{}",
                edge.from_id, edge.relation_type, edge.to_id
            )
        }
        "delete_dataset" => {
            let Some(dataset) = payload.get("dataset") else {
                app.status = "invalid dataset snapshot".into();
                return;
            };
            let dataset: Dataset = match serde_json::from_value(dataset.clone()) {
                Ok(dataset) => dataset,
                Err(error) => {
                    app.status = format!("invalid dataset snapshot: {error}");
                    return;
                }
            };
            let ids = payload
                .get("fact_ids")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>();
            let mut statements = vec![format!(
                "CREATE dataset CONTENT {{ name: {}, description: {}, created_at: time::now() }}",
                sql_string(&dataset.name),
                sql_string(&dataset.description)
            )];
            statements.extend(ids.iter().map(|id| {
                format!(
                    "UPDATE {id} SET dataset = {}, updated_at = time::now()",
                    sql_string(&dataset.name)
                )
            }));
            statements.join("; ")
        }
        _ => {
            app.status = format!("unsupported undo action: {action}");
            return;
        }
    };
    match db_transaction(&format!(
        "{restore_sql}; UPDATE {audit_id} SET undone = true"
    ))
    .await
    {
        Ok(_) => match refresh(app).await {
            Ok(()) => app.status = format!("undid {action}"),
            Err(error) => app.status = format!("undo complete; refresh failed: {error}"),
        },
        Err(error) => app.status = format!("undo failed: {error}"),
    }
}

async fn redo_last(app: &mut App) {
    let rows = match db("SELECT id, action, entity_id, payload, timestamp FROM graph_audit WHERE undone = true ORDER BY timestamp DESC LIMIT 1").await {
        Ok(rows) => rows,
        Err(error) => {
            app.status = format!("redo journal unavailable: {error}");
            return;
        }
    };
    let Some(row) = rows
        .first()
        .and_then(|row| row["result"].as_array())
        .and_then(|items| items.first())
    else {
        app.status = "nothing to redo".into();
        return;
    };
    let (Some(audit_id), Some(action), Some(payload)) = (
        row["id"].as_str(),
        row["action"].as_str(),
        row["payload"].as_str(),
    ) else {
        app.status = "invalid redo journal record".into();
        return;
    };
    let payload: Value = match serde_json::from_str(payload) {
        Ok(payload) => payload,
        Err(error) => {
            app.status = format!("invalid redo payload: {error}");
            return;
        }
    };
    let redo_sql = match action {
        "delete_facts" => {
            let deleted: DeletedFacts = match serde_json::from_value(payload) {
                Ok(deleted) => deleted,
                Err(error) => {
                    app.status = format!("invalid fact snapshot: {error}");
                    return;
                }
            };
            let ids = deleted
                .facts
                .iter()
                .map(|fact| fact.id.clone())
                .collect::<Vec<_>>();
            let records = ids.join(", ");
            let deletes = ids
                .iter()
                .map(|id| format!("DELETE {id}"))
                .collect::<Vec<_>>()
                .join("; ");
            format!(
                "DELETE relates_to WHERE in IN [{records}] OR out IN [{records}]; DELETE proves WHERE in IN [{records}] OR out IN [{records}]; {deletes}"
            )
        }
        "delete_edge" => {
            let edge: Edge = match serde_json::from_value(payload) {
                Ok(edge) => edge,
                Err(error) => {
                    app.status = format!("invalid edge snapshot: {error}");
                    return;
                }
            };
            format!(
                "DELETE {} WHERE in = {} AND out = {}",
                edge.relation_type, edge.from_id, edge.to_id
            )
        }
        "delete_dataset" => {
            let Some(dataset) = payload.get("dataset") else {
                app.status = "invalid dataset snapshot".into();
                return;
            };
            let dataset: Dataset = match serde_json::from_value(dataset.clone()) {
                Ok(dataset) => dataset,
                Err(error) => {
                    app.status = format!("invalid dataset snapshot: {error}");
                    return;
                }
            };
            let quoted = sql_string(&dataset.name);
            format!(
                "UPDATE fact SET dataset = \"default\", updated_at = time::now() WHERE dataset = {quoted}; DELETE dataset WHERE name = {quoted}"
            )
        }
        _ => {
            app.status = format!("unsupported redo action: {action}");
            return;
        }
    };
    match db_transaction(&format!("{redo_sql}; UPDATE {audit_id} SET undone = false")).await {
        Ok(_) => match refresh(app).await {
            Ok(()) => app.status = format!("redid {action}"),
            Err(error) => app.status = format!("redo complete; refresh failed: {error}"),
        },
        Err(error) => app.status = format!("redo failed: {error}"),
    }
}

fn expand_home(path: &str) -> PathBuf {
    if path == "~" || path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(path.trim_start_matches("~/"));
        }
    }
    PathBuf::from(path)
}

fn saved_views_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".uintell")
        .join("graph-views.json")
}

fn valid_saved_view(view: &SavedView) -> bool {
    valid_label(&view.name)
        && (view.dataset_filter == "all" || valid_label(&view.dataset_filter))
        && (view.type_filter == "all" || valid_label(&view.type_filter))
        && view.center_x.is_finite()
        && view.center_y.is_finite()
        && (0.0..=1.0).contains(&view.center_x)
        && (0.0..=1.0).contains(&view.center_y)
        && view.zoom.is_finite()
        && (0.5..=6.0).contains(&view.zoom)
}

fn load_saved_views() -> Result<Vec<SavedView>, String> {
    let path = saved_views_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = std::fs::read_to_string(&path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let mut views: Vec<SavedView> = serde_json::from_str(&contents)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if views.len() > 500 || !views.iter().all(valid_saved_view) {
        return Err("saved graph views contain invalid data".into());
    }
    views.sort_by(|left, right| left.name.cmp(&right.name));
    views.dedup_by(|left, right| left.name == right.name);
    Ok(views)
}

fn persist_saved_views(views: &[SavedView]) -> Result<(), String> {
    let path = saved_views_path();
    let parent = path
        .parent()
        .ok_or_else(|| "saved-view path has no parent".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create {}: {error}", parent.display()))?;
    let temporary = path.with_extension("json.tmp");
    let contents = serde_json::to_vec_pretty(views)
        .map_err(|error| format!("serialize saved views: {error}"))?;
    std::fs::write(&temporary, contents)
        .map_err(|error| format!("write {}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, &path)
        .map_err(|error| format!("replace {}: {error}", path.display()))
}

fn save_view(app: &mut App, name: Option<&str>) {
    let name = name.unwrap_or_default().trim();
    if !valid_label(name) {
        app.status = "usage: :view-save <name>".into();
        return;
    }
    let previous = app.saved_views.clone();
    let view = SavedView {
        name: name.into(),
        dataset_filter: app.dataset_filter.clone(),
        type_filter: app.type_filter.clone(),
        center_x: app.center_x,
        center_y: app.center_y,
        zoom: app.zoom,
    };
    if let Some(existing) = app.saved_views.iter_mut().find(|view| view.name == name) {
        *existing = view;
    } else {
        app.saved_views.push(view);
        app.saved_views
            .sort_by(|left, right| left.name.cmp(&right.name));
    }
    match persist_saved_views(&app.saved_views) {
        Ok(()) => app.status = format!("saved graph view {name}"),
        Err(error) => {
            app.saved_views = previous;
            app.status = format!("save view failed: {error}");
        }
    }
}

fn activate_view(app: &mut App, name: Option<&str>) {
    let name = name.unwrap_or_default().trim();
    let Some(view) = app
        .saved_views
        .iter()
        .find(|view| view.name == name)
        .cloned()
    else {
        app.status = if name.is_empty() {
            "usage: :view <name>".into()
        } else {
            format!("unknown graph view: {name}")
        };
        return;
    };
    app.dataset_filter = view.dataset_filter;
    app.type_filter = view.type_filter;
    app.center_x = view.center_x;
    app.center_y = view.center_y;
    app.zoom = view.zoom;
    select_endpoint(app, false);
    app.status = format!("restored graph view {name}");
}

fn delete_view(app: &mut App, name: Option<&str>) {
    let name = name.unwrap_or_default().trim();
    let previous = app.saved_views.clone();
    app.saved_views.retain(|view| view.name != name);
    if app.saved_views.len() == previous.len() {
        app.status = format!("unknown graph view: {name}");
        return;
    }
    match persist_saved_views(&app.saved_views) {
        Ok(()) => app.status = format!("deleted graph view {name}"),
        Err(error) => {
            app.saved_views = previous;
            app.status = format!("delete view failed: {error}");
        }
    }
}

fn show_views(app: &mut App) {
    app.query_output = if app.saved_views.is_empty() {
        "SAVED GRAPH VIEWS\n\nNo saved views.".into()
    } else {
        format!(
            "SAVED GRAPH VIEWS\n\n{}",
            app.saved_views
                .iter()
                .map(|view| format!(
                    "{}  dataset={} type={} center={:.3},{:.3} zoom={:.1}x",
                    view.name,
                    view.dataset_filter,
                    view.type_filter,
                    view.center_x,
                    view.center_y,
                    view.zoom
                ))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    app.query_scroll = 0;
    app.tab = Tab::Query;
    app.status = format!("{} saved graph view(s)", app.saved_views.len());
}

fn default_export_path() -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".uintell")
        .join(format!("graph-export-{timestamp}.json"))
}

fn export_graph(app: &App, path: Option<&str>) -> Result<PathBuf, String> {
    let path = path
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(expand_home)
        .unwrap_or_else(default_export_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("create export dir: {error}"))?;
    }
    let snapshot = ExportSnapshot {
        version: 1,
        namespace: "agent".into(),
        database: "graph".into(),
        exported_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        total_facts: app.total_facts,
        partial: app.total_facts > app.facts.len() as u64,
        datasets: app.datasets.clone(),
        facts: app.facts.clone(),
        edges: app.edges.clone(),
    };
    let json = serde_json::to_string_pretty(&snapshot)
        .map_err(|error| format!("serialize export: {error}"))?;
    std::fs::write(&path, json).map_err(|error| format!("write export: {error}"))?;
    Ok(path)
}

fn load_import_snapshot(path: &str) -> Result<(PathBuf, ExportSnapshot), String> {
    let path = expand_home(path.trim());
    let contents = std::fs::read_to_string(&path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let snapshot: ExportSnapshot = serde_json::from_str(&contents)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if snapshot.version != 1 {
        return Err(format!(
            "unsupported snapshot version: {}",
            snapshot.version
        ));
    }
    if snapshot.namespace != "agent" || snapshot.database != "graph" {
        return Err("snapshot namespace/database does not match agent/graph".into());
    }
    if snapshot.total_facts != 0 && snapshot.total_facts < snapshot.facts.len() as u64 {
        return Err("snapshot total_facts is smaller than its fact records".into());
    }
    if !snapshot
        .datasets
        .iter()
        .all(|dataset| valid_label(&dataset.name))
    {
        return Err("snapshot contains an invalid dataset name".into());
    }
    if !snapshot.facts.iter().all(|fact| {
        valid_fact_id(&fact.id)
            && valid_label(&fact.fact_type)
            && valid_label(&fact.dataset)
            && fact.tags.iter().all(|tag| valid_label(tag))
            && (0.0..=1.0).contains(&fact.confidence)
            && fact.graph_x.is_finite()
            && fact.graph_y.is_finite()
    }) {
        return Err("snapshot contains an invalid knowledge unit".into());
    }
    let fact_ids = snapshot
        .facts
        .iter()
        .map(|fact| fact.id.as_str())
        .collect::<BTreeSet<_>>();
    if !snapshot.edges.iter().all(|edge| {
        valid_edge_id(&edge.id)
            && matches!(edge.relation_type.as_str(), "relates_to" | "proves")
            && valid_fact_id(&edge.from_id)
            && valid_fact_id(&edge.to_id)
            && fact_ids.contains(edge.from_id.as_str())
            && fact_ids.contains(edge.to_id.as_str())
    }) {
        return Err("snapshot contains an invalid or dangling relation".into());
    }
    Ok((path, snapshot))
}

async fn import_graph(app: &mut App, path: Option<&str>, apply: bool) {
    let Some(path) = path.map(str::trim).filter(|path| !path.is_empty()) else {
        app.status = "usage: :import <path> or :import! <path>".into();
        return;
    };
    let (path, snapshot) = match load_import_snapshot(path) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            app.status = format!("import rejected: {error}");
            return;
        }
    };
    let existing_ids = app
        .facts
        .iter()
        .map(|fact| fact.id.as_str())
        .collect::<BTreeSet<_>>();
    let new_facts = snapshot
        .facts
        .iter()
        .filter(|fact| !existing_ids.contains(fact.id.as_str()))
        .collect::<Vec<_>>();
    let existing_datasets = app
        .datasets
        .iter()
        .map(|dataset| dataset.name.as_str())
        .collect::<BTreeSet<_>>();
    let new_datasets = snapshot
        .datasets
        .iter()
        .filter(|dataset| !existing_datasets.contains(dataset.name.as_str()))
        .collect::<Vec<_>>();
    let new_edges = snapshot
        .edges
        .iter()
        .filter(|edge| {
            !app.edges.iter().any(|existing| {
                existing.relation_type == edge.relation_type
                    && existing.from_id == edge.from_id
                    && existing.to_id == edge.to_id
            })
        })
        .collect::<Vec<_>>();
    if !apply {
        app.query_output = format!(
            "SNAPSHOT PREVIEW\n\nPath: {}\nVersion: {}\nExported: {}\nScope: {} of {} facts{}\n\n{} datasets ({} new)\n{} facts ({} new)\n{} edges ({} new)\n\nNo data changed. Run :import! {} to merge new records.",
            path.display(),
            snapshot.version,
            snapshot.exported_at_unix,
            snapshot.facts.len(),
            snapshot.total_facts.max(snapshot.facts.len() as u64),
            if snapshot.partial { " (partial)" } else { "" },
            snapshot.datasets.len(),
            new_datasets.len(),
            snapshot.facts.len(),
            new_facts.len(),
            snapshot.edges.len(),
            new_edges.len(),
            path.display()
        );
        app.query_scroll = 0;
        app.tab = Tab::Query;
        app.status = "snapshot validated; preview only".into();
        return;
    }
    let mut statements = new_datasets
        .iter()
        .map(|dataset| {
            format!(
                "CREATE dataset CONTENT {{ name: {}, description: {}, created_at: time::now() }}",
                sql_string(&dataset.name),
                sql_string(&dataset.description)
            )
        })
        .collect::<Vec<_>>();
    statements.extend(new_facts.iter().map(|fact| fact_restore_sql(fact)));
    statements.extend(new_edges.iter().map(|edge| {
        format!(
            "RELATE {}->{}->{}",
            edge.from_id, edge.relation_type, edge.to_id
        )
    }));
    if statements.is_empty() {
        app.status = "snapshot already fully present; nothing imported".into();
        return;
    }
    match db_transaction(&statements.join("; ")).await {
        Ok(_) => match refresh(app).await {
            Ok(()) => {
                app.status = format!(
                    "imported {} datasets, {} facts, {} edges",
                    new_datasets.len(),
                    new_facts.len(),
                    new_edges.len()
                )
            }
            Err(error) => app.status = format!("imported; refresh failed: {error}"),
        },
        Err(error) => app.status = format!("import failed: {error}"),
    }
}

async fn repair_metadata(app: &mut App) {
    let mut statements = Vec::new();
    let stored = match db("SELECT name FROM dataset").await {
        Ok(rows) => rows
            .first()
            .and_then(|row| row["result"].as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|row| row["name"].as_str().map(String::from))
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default(),
        Err(error) => {
            app.status = format!("repair scan failed: {error}");
            return;
        }
    };
    let mut names = app
        .facts
        .iter()
        .map(|fact| fact.dataset.as_str())
        .collect::<BTreeSet<_>>();
    names.insert("default");
    for name in names {
        if !stored.contains(name) {
            statements.push(format!(
                "CREATE dataset CONTENT {{ name: {}, description: \"\", created_at: time::now() }}",
                sql_string(name)
            ));
        }
    }
    let mut seen = BTreeSet::new();
    for edge in &app.edges {
        let key = format!("{}|{}|{}", edge.relation_type, edge.from_id, edge.to_id);
        if !seen.insert(key) {
            statements.push(format!("DELETE {}", edge.id));
        }
    }
    if statements.is_empty() {
        app.status = "integrity check passed; no repairs needed".into();
        return;
    }
    match db_transaction(&statements.join("; ")).await {
        Ok(_) => match refresh(app).await {
            Ok(()) => app.status = format!("applied {} integrity repairs", statements.len()),
            Err(error) => app.status = format!("repaired; refresh failed: {error}"),
        },
        Err(error) => app.status = format!("repair failed: {error}"),
    }
}

fn is_read_only_sql(sql: &str) -> bool {
    sql.split(';')
        .filter(|part| !part.trim().is_empty())
        .all(|part| {
            let keyword = part
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase();
            matches!(
                keyword.as_str(),
                "SELECT" | "RETURN" | "INFO" | "SHOW" | "SLEEP"
            )
        })
}

fn execute_query(app: &mut App, sql: &str, allow_write: bool) {
    let sql = sql.trim();
    if sql.is_empty() {
        app.status = "query cannot be empty".into();
        return;
    }
    if !allow_write && !is_read_only_sql(sql) {
        app.status = "mutating SurrealQL requires :query! <sql>".into();
        return;
    }
    if allow_write {
        if app.active_job.is_some() {
            mutation_blocked(app);
            return;
        }
        app.confirm = Some(ConfirmAction::Query(sql.into()));
        app.mode = Mode::Confirm;
        app.status = "review the mutating query before confirmation".into();
        return;
    }
    queue_query(app, sql, false);
}

fn queue_query(app: &mut App, sql: &str, allow_write: bool) {
    app.query_history.retain(|existing| existing != sql);
    app.query_history.push_front(sql.into());
    app.query_history.truncate(100);
    app.history_selected = 0;
    app.query_scroll = 0;
    start_graph_job(
        app,
        GraphJobKind::Query {
            sql: sql.into(),
            allow_write,
        },
    );
}

fn help_text() -> &'static str {
    "UINTELL GRAPH OPERATIONS CONSOLE

GLOBAL
  Esc/Ctrl+G    cancel the active background job

VIEWS
  1 Graph       visual topology, drag, zoom, pan, layout
  2 Explorer    datasets/types, bulk selection, node inspector
  3 Query       safe SurrealQL workbench, templates, history
  4 Analytics   health, distributions, integrity metrics

GRAPH / EXPLORER
  j/k           select visible node
  arrows        move node (Shift+arrows pan viewport)
  +/- / 0       zoom in/out/reset viewport
  mouse drag    reposition and persist node
  Shift+drag    lasso-add nodes to bulk selection
  mouse wheel   zoom
  f / F         next dataset / type filter
  a             topology-aware auto-layout (pinned nodes stay)
  z / Z         fit selected / all visible nodes
  p             pin/unpin selected node
  Space         mark node for bulk actions
  x             clear marked nodes
  c / e         create / edit node
  d             confirm delete selected or marked nodes
  l             select link source, then target
  t             toggle relates_to / proves
  [ / ]         select connected edge
  u             confirm selected edge removal
  U             undo last destructive operation

COMMANDS
  :query <sql>                 read-only SurrealQL
  :query! <sql>                explicitly allow mutations
  :schema                      show database schema
  :dataset <all|name>          filter
  :dataset-new <name> | <desc>
  :dataset-delete <name>       confirmed and undoable
  :move <dataset>              move selected/marked nodes
  :layout                      auto-layout visible graph
  :fit / :fit-all              fit selected / visible nodes
  :view-save <name>            persist filters and viewport
  :view <name>                 restore a saved graph view
  :views                       list saved graph views
  :view-delete <name>          remove a saved graph view
  :repair                      add missing datasets, dedupe edges
  :export [path]               JSON graph snapshot
  :import <path>               validate and preview snapshot
  :import! <path>              merge validated new records
  :undo                        undo last destructive operation
  :redo                        redo last undone operation
  :refresh                     reload SurrealDB in background
  :load-more [count]           raise the loaded-node window (max 100000)
  :load-all                    load up to 100000 nodes
  :cancel                      cancel the active background job
  :retry                       retry the last safe background job
  :help                        this reference"
}

async fn create_dataset(app: &mut App, input: Option<&str>) {
    let Some(input) = input else {
        app.status = "usage: :dataset-new <name> | [description]".into();
        return;
    };
    let mut fields = input.splitn(2, '|').map(str::trim);
    let name = fields.next().unwrap_or_default();
    let description = fields.next().unwrap_or_default();
    if !valid_label(name) {
        app.status = "invalid dataset name".into();
        return;
    }
    if app.datasets.iter().any(|dataset| dataset.name == name) {
        app.status = format!("dataset already exists: {name}");
        return;
    }
    match db(&format!(
        "CREATE dataset CONTENT {{ name: {}, description: {}, created_at: time::now() }}",
        sql_string(name),
        sql_string(description)
    ))
    .await
    {
        Ok(_) => match refresh(app).await {
            Ok(()) => {
                app.dataset_filter = name.into();
                select_endpoint(app, false);
                app.status = format!("created dataset {name}");
            }
            Err(error) => app.status = format!("created; refresh failed: {error}"),
        },
        Err(error) => app.status = format!("dataset create failed: {error}"),
    }
}

async fn move_units(app: &mut App, dataset: Option<&str>) {
    let dataset = dataset.unwrap_or_default().trim();
    if !valid_label(dataset) {
        app.status = "usage: :move <dataset>".into();
        return;
    }
    let ids = if app.marked.is_empty() {
        selected_fact(app)
            .map(|fact| vec![fact.id.clone()])
            .unwrap_or_default()
    } else {
        app.marked.iter().cloned().collect::<Vec<_>>()
    };
    if ids.is_empty() {
        app.status = "no selected or marked units".into();
        return;
    }
    let statements = ids
        .iter()
        .map(|id| {
            format!(
                "UPDATE {id} SET dataset = {}, updated_at = time::now()",
                sql_string(dataset)
            )
        })
        .collect::<Vec<_>>();
    match db_transaction(&statements.join("; ")).await {
        Ok(_) => {
            app.marked.clear();
            match refresh(app).await {
                Ok(()) => app.status = format!("moved {} unit(s) to {dataset}", ids.len()),
                Err(error) => app.status = format!("moved; refresh failed: {error}"),
            }
        }
        Err(error) => app.status = format!("move failed: {error}"),
    }
}

async fn execute_command(app: &mut App, command: &str) -> bool {
    let parts = command.trim().splitn(2, ' ').collect::<Vec<_>>();
    let name = parts.first().copied().unwrap_or_default();
    let argument = parts.get(1).copied();
    if matches!(
        name,
        "dataset-new" | "dataset-delete" | "move" | "repair" | "import!" | "undo" | "redo"
    ) && mutation_blocked(app)
    {
        return false;
    }
    match name {
        "q" | "quit" => return true,
        "r" | "refresh" => start_graph_job(app, GraphJobKind::Refresh),
        "query" => {
            let sql = argument.unwrap_or_default();
            app.query_editor = new_query_editor(sql);
            app.query_focus = QueryFocus::Results;
            app.tab = Tab::Query;
            execute_query(app, sql, false);
        }
        "query!" => {
            let sql = argument.unwrap_or_default();
            app.query_editor = new_query_editor(sql);
            app.query_focus = QueryFocus::Results;
            app.tab = Tab::Query;
            execute_query(app, sql, true);
        }
        "schema" => {
            app.query_editor = new_query_editor("INFO FOR DB");
            app.query_focus = QueryFocus::Results;
            app.tab = Tab::Query;
            execute_query(app, "INFO FOR DB", false);
        }
        "help" => {
            app.query_output = help_text().into();
            app.query_scroll = 0;
            app.tab = Tab::Query;
            app.status = "command reference".into();
        }
        "dataset" => {
            let requested = argument.unwrap_or("all").trim();
            if requested == "all" || app.datasets.iter().any(|dataset| dataset.name == requested) {
                app.dataset_filter = requested.into();
                select_endpoint(app, false);
                app.status = format!("dataset filter: {requested}");
            } else {
                app.status = format!("unknown dataset: {requested}");
            }
        }
        "dataset-new" => create_dataset(app, argument).await,
        "dataset-delete" => {
            let requested = argument.unwrap_or_default().trim();
            if requested == "default" || !valid_label(requested) {
                app.status = "provide a non-default dataset name".into();
            } else if !app.datasets.iter().any(|dataset| dataset.name == requested) {
                app.status = format!("unknown dataset: {requested}");
            } else {
                app.confirm = Some(ConfirmAction::Dataset(requested.into()));
                app.mode = Mode::Confirm;
            }
        }
        "move" => move_units(app, argument).await,
        "layout" => auto_layout(app),
        "fit" => fit_selection(app, false),
        "fit-all" => fit_selection(app, true),
        "view-save" => save_view(app, argument),
        "view" => activate_view(app, argument),
        "views" => show_views(app),
        "view-delete" => delete_view(app, argument),
        "load-more" => load_more(app, argument, false),
        "load-all" => load_more(app, None, true),
        "cancel" => cancel_graph_job(app),
        "retry" => retry_graph_job(app),
        "repair" => repair_metadata(app).await,
        "export" => match export_graph(app, argument) {
            Ok(path) => {
                app.status = if app.total_facts > app.facts.len() as u64 {
                    format!(
                        "exported partial graph ({}/{}) to {}",
                        app.facts.len(),
                        app.total_facts,
                        path.display()
                    )
                } else {
                    format!("exported graph to {}", path.display())
                }
            }
            Err(error) => app.status = format!("export failed: {error}"),
        },
        "import" => import_graph(app, argument, false).await,
        "import!" => import_graph(app, argument, true).await,
        "undo" => undo_last(app).await,
        "redo" => redo_last(app).await,
        "clear" => {
            app.query_output.clear();
            app.query_scroll = 0;
            app.status = "query output cleared".into();
        }
        "" => {}
        _ => app.status = format!("unknown command: {command}"),
    }
    false
}

fn search(app: &mut App, query: &str) {
    let query = query.to_lowercase();
    let found = visible_indices(app).into_iter().find(|index| {
        let fact = &app.facts[*index];
        fact.content.to_lowercase().contains(&query)
            || fact.fact_type.to_lowercase().contains(&query)
            || fact.dataset.to_lowercase().contains(&query)
            || fact
                .tags
                .iter()
                .any(|tag| tag.to_lowercase().contains(&query))
    });
    if let Some(index) = found {
        app.selected = index;
        app.edge_selected = 0;
        app.status = format!("found {}", app.facts[index].id);
    } else {
        app.status = format!("no visible match: {query}");
    }
}

fn begin_delete_facts(app: &mut App) {
    if mutation_blocked(app) {
        return;
    }
    let ids = if app.marked.is_empty() {
        selected_fact(app)
            .map(|fact| vec![fact.id.clone()])
            .unwrap_or_default()
    } else {
        app.marked.iter().cloned().collect::<Vec<_>>()
    };
    if ids.is_empty() {
        app.status = "no selected or marked units".into();
        return;
    }
    app.confirm = Some(ConfirmAction::Facts(ids));
    app.mode = Mode::Confirm;
}

fn begin_delete_edge(app: &mut App) {
    if mutation_blocked(app) {
        return;
    }
    let connected = connected_edges(app);
    let Some(index) = connected.get(app.edge_selected) else {
        app.status = "no connected relation selected".into();
        return;
    };
    app.confirm = Some(ConfirmAction::Edge(app.edges[*index].id.clone()));
    app.mode = Mode::Confirm;
}

fn toggle_mark(app: &mut App) {
    let Some(id) = selected_fact(app).map(|fact| fact.id.clone()) else {
        return;
    };
    if !app.marked.remove(&id) {
        app.marked.insert(id);
    }
    select_offset(app, 1);
    app.status = format!("{} marked unit(s)", app.marked.len());
}

fn console_content_area(area: Rect) -> Option<Rect> {
    if area.width == 0 || area.height < 5 {
        return None;
    }
    Some(Rect::new(
        area.x,
        area.y.saturating_add(3),
        area.width,
        area.height.saturating_sub(5),
    ))
}

fn mouse_canvas(area: Rect) -> Option<Rect> {
    if area.width < 40 || area.height < 10 {
        return None;
    }
    let content = console_content_area(area)?;
    let (graph, _) = graph_panels(content);
    Some(graph_canvas(graph))
}

fn rect_has(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

fn handle_query_mouse(app: &mut App, mouse: MouseEvent, area: Rect) {
    let Some(content) = console_content_area(area) else {
        return;
    };
    let layout = query_layout(content);
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if rect_has(layout.templates, mouse.column, mouse.row) {
                let index = mouse
                    .row
                    .saturating_sub(layout.templates.y.saturating_add(1))
                    as usize;
                if index < QUERY_TEMPLATES.len() {
                    app.template_selected = index;
                    begin_query(app, QUERY_TEMPLATES[index].1);
                }
            } else if rect_has(layout.history, mouse.column, mouse.row) {
                let height = layout.history.height.saturating_sub(2) as usize;
                let offset = query_history_offset(app, height);
                let index = offset.saturating_add(
                    mouse.row.saturating_sub(layout.history.y.saturating_add(1)) as usize,
                );
                if let Some(sql) = app.query_history.get(index).cloned() {
                    app.history_selected = index;
                    begin_query(app, &sql);
                }
            } else if rect_has(layout.editor, mouse.column, mouse.row) {
                app.query_focus = QueryFocus::Editor;
                app.mode = Mode::QueryEdit;
                let inner = layout.editor.inner(Margin {
                    horizontal: 1,
                    vertical: 1,
                });
                let line_number_width = app.query_editor.lines().len().to_string().len() + 2;
                let row = mouse.row.saturating_sub(inner.y);
                let col = mouse
                    .column
                    .saturating_sub(inner.x)
                    .saturating_sub(line_number_width as u16);
                app.query_editor.move_cursor(CursorMove::Jump(row, col));
                app.status = "editing SurrealQL".into();
            } else if rect_has(layout.results, mouse.column, mouse.row) {
                app.query_focus = QueryFocus::Results;
                app.mode = Mode::Normal;
            }
        }
        MouseEventKind::ScrollUp => {
            if rect_has(layout.templates, mouse.column, mouse.row) {
                app.query_focus = QueryFocus::Templates;
                app.template_selected = app.template_selected.saturating_sub(1);
            } else if rect_has(layout.history, mouse.column, mouse.row) {
                app.query_focus = QueryFocus::History;
                app.history_selected = app.history_selected.saturating_sub(1);
            } else if rect_has(layout.results, mouse.column, mouse.row) {
                app.query_focus = QueryFocus::Results;
                app.query_scroll = app.query_scroll.saturating_sub(3);
            } else if rect_has(layout.editor, mouse.column, mouse.row) {
                app.query_focus = QueryFocus::Editor;
                app.query_editor.scroll((-3, 0));
            }
        }
        MouseEventKind::ScrollDown => {
            if rect_has(layout.templates, mouse.column, mouse.row) {
                app.query_focus = QueryFocus::Templates;
                app.template_selected = app
                    .template_selected
                    .saturating_add(1)
                    .min(QUERY_TEMPLATES.len().saturating_sub(1));
            } else if rect_has(layout.history, mouse.column, mouse.row) {
                app.query_focus = QueryFocus::History;
                app.history_selected = app
                    .history_selected
                    .saturating_add(1)
                    .min(app.query_history.len().saturating_sub(1));
            } else if rect_has(layout.results, mouse.column, mouse.row) {
                app.query_focus = QueryFocus::Results;
                app.query_scroll = app.query_scroll.saturating_add(3);
            } else if rect_has(layout.editor, mouse.column, mouse.row) {
                app.query_focus = QueryFocus::Editor;
                app.query_editor.scroll((3, 0));
            }
        }
        _ => {}
    }
}

fn update_position_from_mouse(app: &mut App, column: u16, row: u16, canvas: Rect) {
    if selected_fact(app).is_none() {
        return;
    }
    let view_x = f64::from(column.saturating_sub(canvas.x))
        / f64::from(canvas.width.saturating_sub(1).max(1));
    let view_y =
        f64::from(row.saturating_sub(canvas.y)) / f64::from(canvas.height.saturating_sub(1).max(1));
    let fact = &mut app.facts[app.selected];
    fact.graph_x = ((view_x - 0.5) / app.zoom + app.center_x).clamp(0.0, 1.0);
    fact.graph_y = ((view_y - 0.5) / app.zoom + app.center_y).clamp(0.0, 1.0);
}

fn canvas_point(column: u16, row: u16, canvas: Rect) -> (u16, u16) {
    (
        column.clamp(canvas.x, canvas.right().saturating_sub(1).max(canvas.x)),
        row.clamp(canvas.y, canvas.bottom().saturating_sub(1).max(canvas.y)),
    )
}

fn finish_lasso(app: &mut App, canvas: Rect) {
    let (Some(start), Some(end)) = (app.lasso_start.take(), app.lasso_end.take()) else {
        return;
    };
    let left = start.0.min(end.0);
    let right = start.0.max(end.0);
    let top = start.1.min(end.1);
    let bottom = start.1.max(end.1);
    let hits = viewport_indices(app)
        .into_iter()
        .filter(|index| {
            screen_position(app, &app.facts[*index], canvas)
                .is_some_and(|(x, y)| (left..=right).contains(&x) && (top..=bottom).contains(&y))
        })
        .collect::<Vec<_>>();
    if let Some(first) = hits.first() {
        app.selected = *first;
        app.edge_selected = 0;
    }
    for index in &hits {
        app.marked.insert(app.facts[*index].id.clone());
    }
    app.status = format!(
        "lasso selected {} unit(s); {} marked total",
        hits.len(),
        app.marked.len()
    );
}

fn graph_node_at(app: &App, column: u16, row: u16, canvas: Rect) -> Option<usize> {
    viewport_indices(app).into_iter().rev().find(|index| {
        let fact = &app.facts[*index];
        let Some((x, y)) = screen_position(app, fact, canvas) else {
            return false;
        };
        let label_width = 2 + trunc(&fact.content.replace('\n', " "), 18).chars().count();
        row == y
            && column >= x.saturating_sub(1)
            && usize::from(column.saturating_sub(x)) <= label_width
    })
}

async fn handle_mouse(app: &mut App, mouse: MouseEvent, area: Rect) {
    if app.tab == Tab::Query {
        handle_query_mouse(app, mouse, area);
        return;
    }
    if app.tab != Tab::Graph || app.mode != Mode::Normal {
        return;
    }
    let Some(canvas) = mouse_canvas(area) else {
        return;
    };
    if app.active_job.is_some()
        && !mouse.modifiers.contains(KeyModifiers::SHIFT)
        && matches!(
            mouse.kind,
            MouseEventKind::Down(MouseButton::Left)
                | MouseEventKind::Drag(MouseButton::Left)
                | MouseEventKind::Up(MouseButton::Left)
        )
    {
        mutation_blocked(app);
        return;
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left)
            if mouse.modifiers.contains(KeyModifiers::SHIFT) =>
        {
            let point = canvas_point(mouse.column, mouse.row, canvas);
            app.dragging = false;
            app.lasso_start = Some(point);
            app.lasso_end = Some(point);
            app.status = "lasso selecting; release to mark units".into();
        }
        MouseEventKind::Drag(MouseButton::Left) if app.lasso_start.is_some() => {
            app.lasso_end = Some(canvas_point(mouse.column, mouse.row, canvas));
        }
        MouseEventKind::Up(MouseButton::Left) if app.lasso_start.is_some() => {
            app.lasso_end = Some(canvas_point(mouse.column, mouse.row, canvas));
            finish_lasso(app, canvas);
        }
        MouseEventKind::Down(MouseButton::Left) => {
            app.lasso_start = None;
            app.lasso_end = None;
            if let Some(index) = graph_node_at(app, mouse.column, mouse.row, canvas) {
                app.selected = index;
                app.edge_selected = 0;
                app.dragging = true;
            }
        }
        MouseEventKind::Down(MouseButton::Right) => {
            if let Some(index) = graph_node_at(app, mouse.column, mouse.row, canvas) {
                app.selected = index;
                app.edge_selected = 0;
                open_edit(app);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) if app.dragging => {
            update_position_from_mouse(app, mouse.column, mouse.row, canvas);
        }
        MouseEventKind::Up(MouseButton::Left) if app.dragging => {
            update_position_from_mouse(app, mouse.column, mouse.row, canvas);
            app.dragging = false;
            if let Some(fact) = selected_fact(app).cloned() {
                match persist_position(&fact).await {
                    Ok(()) => {
                        rebuild_graph_caches(app);
                        app.status = format!(
                            "positioned {} at {:.3}, {:.3}",
                            fact.id, fact.graph_x, fact.graph_y
                        )
                    }
                    Err(error) => app.status = format!("position failed: {error}"),
                }
            }
        }
        MouseEventKind::ScrollUp => {
            app.zoom = (app.zoom * 1.15).clamp(0.5, 6.0);
            app.status = format!("zoom {:.1}x", app.zoom);
        }
        MouseEventKind::ScrollDown => {
            app.zoom = (app.zoom / 1.15).clamp(0.5, 6.0);
            app.status = format!("zoom {:.1}x", app.zoom);
        }
        _ => {}
    }
}

fn begin_query(app: &mut App, sql: &str) {
    app.query_editor = new_query_editor(sql);
    app.query_focus = QueryFocus::Editor;
    app.mode = Mode::QueryEdit;
    app.status = "editing SurrealQL".into();
}

fn query_editor_sql(app: &App) -> String {
    app.query_editor.lines().join("\n")
}

fn execute_query_editor(app: &mut App, allow_write: bool) {
    let sql = query_editor_sql(app);
    if sql.trim().is_empty() {
        app.status = "query cannot be empty".into();
        return;
    }
    if !allow_write && !is_read_only_sql(&sql) {
        app.status = "this query mutates data; press F6 to review and confirm".into();
        return;
    }
    if app.active_job.is_some() {
        app.status = "another graph operation is active; Esc/Ctrl+G cancels it".into();
        return;
    }

    execute_query(app, &sql, allow_write);
    app.query_focus = QueryFocus::Results;
    if app.mode != Mode::Confirm {
        app.mode = Mode::Normal;
    }
}

fn move_query_selection(app: &mut App, delta: isize) {
    match app.query_focus {
        QueryFocus::Templates => {
            app.template_selected = app
                .template_selected
                .saturating_add_signed(delta)
                .min(QUERY_TEMPLATES.len().saturating_sub(1));
        }
        QueryFocus::History => {
            app.history_selected = app
                .history_selected
                .saturating_add_signed(delta)
                .min(app.query_history.len().saturating_sub(1));
        }
        QueryFocus::Editor => {}
        QueryFocus::Results => {
            if delta.is_negative() {
                app.query_scroll = app.query_scroll.saturating_sub(delta.unsigned_abs());
            } else {
                app.query_scroll = app.query_scroll.saturating_add(delta as usize);
            }
        }
    }
}

fn handle_query_normal_key(app: &mut App, key: KeyEvent) {
    match (key.modifiers, key.code) {
        (KeyModifiers::NONE, KeyCode::Tab) => {
            app.query_focus = app.query_focus.next(false);
        }
        (_, KeyCode::BackTab) => {
            app.query_focus = app.query_focus.next(true);
        }
        (KeyModifiers::NONE, KeyCode::Left) => {
            app.query_focus = app.query_focus.next(true);
        }
        (KeyModifiers::NONE, KeyCode::Right) => {
            app.query_focus = app.query_focus.next(false);
        }
        (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down) => {
            move_query_selection(app, 1);
        }
        (KeyModifiers::NONE, KeyCode::Char('k') | KeyCode::Up) => {
            move_query_selection(app, -1);
        }
        (_, KeyCode::PageDown) => {
            app.query_focus = QueryFocus::Results;
            app.query_scroll = app.query_scroll.saturating_add(5);
        }
        (_, KeyCode::PageUp) => {
            app.query_focus = QueryFocus::Results;
            app.query_scroll = app.query_scroll.saturating_sub(5);
        }
        (KeyModifiers::NONE, KeyCode::Char('t')) => {
            app.query_focus = QueryFocus::Templates;
        }
        (KeyModifiers::NONE, KeyCode::Char('h')) => {
            app.query_focus = QueryFocus::History;
        }
        (KeyModifiers::NONE, KeyCode::Char('e')) => {
            app.query_focus = QueryFocus::Editor;
            app.mode = Mode::QueryEdit;
        }
        (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
            app.query_editor = new_query_editor("");
            app.query_focus = QueryFocus::Editor;
            app.mode = Mode::QueryEdit;
        }
        (KeyModifiers::NONE, KeyCode::Enter) => match app.query_focus {
            QueryFocus::Templates => {
                begin_query(app, QUERY_TEMPLATES[app.template_selected].1);
            }
            QueryFocus::History => {
                if let Some(sql) = app.query_history.get(app.history_selected).cloned() {
                    begin_query(app, &sql);
                } else {
                    app.status = "query history is empty".into();
                }
            }
            QueryFocus::Editor => app.mode = Mode::QueryEdit,
            QueryFocus::Results => {}
        },
        (KeyModifiers::NONE, KeyCode::F(5))
        | (KeyModifiers::CONTROL, KeyCode::Enter)
        | (KeyModifiers::ALT, KeyCode::Enter) => execute_query_editor(app, false),
        (KeyModifiers::NONE, KeyCode::F(6)) => execute_query_editor(app, true),
        _ => {}
    }
}

async fn handle_normal_key(app: &mut App, key: KeyEvent) -> bool {
    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
        return true;
    }
    if key.modifiers == KeyModifiers::NONE {
        match key.code {
            KeyCode::Char('1') => {
                app.tab = Tab::Graph;
                return false;
            }
            KeyCode::Char('2') => {
                app.tab = Tab::Explorer;
                return false;
            }
            KeyCode::Char('3') => {
                app.tab = Tab::Query;
                return false;
            }
            KeyCode::Char('4') => {
                app.tab = Tab::Analytics;
                return false;
            }
            _ => {}
        }
    }
    match (key.modifiers, key.code) {
        (_, KeyCode::Char('q')) => return true,
        (KeyModifiers::NONE, KeyCode::Char(':')) => {
            app.mode = Mode::Command;
            app.command.clear();
        }
        (KeyModifiers::NONE, KeyCode::Char('/')) => {
            app.mode = Mode::Search;
            app.command.clear();
        }
        (KeyModifiers::NONE, KeyCode::Char('r')) => start_graph_job(app, GraphJobKind::Refresh),
        (KeyModifiers::SHIFT, KeyCode::Char('U')) => {
            if !mutation_blocked(app) {
                undo_last(app).await;
            }
        }
        (KeyModifiers::SHIFT, KeyCode::Char('R')) => {
            if !mutation_blocked(app) {
                redo_last(app).await;
            }
        }
        (KeyModifiers::NONE, KeyCode::Esc) => {
            if app.active_job.is_some() {
                cancel_graph_job(app);
            } else {
                app.link_source = None;
                app.status = "link cancelled".into();
            }
        }
        _ => match app.tab {
            Tab::Graph | Tab::Explorer => match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Char('j')) => select_offset(app, 1),
                (KeyModifiers::NONE, KeyCode::Char('k')) => select_offset(app, -1),
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    if app.tab == Tab::Graph {
                        app.tab = Tab::Explorer;
                        app.status = "selected unit opened in Explorer".into();
                    } else {
                        open_edit(app);
                    }
                }
                (KeyModifiers::NONE, KeyCode::Home) => select_endpoint(app, false),
                (KeyModifiers::NONE, KeyCode::End) => select_endpoint(app, true),
                (KeyModifiers::NONE, KeyCode::Left) => move_selected(app, -0.02, 0.0).await,
                (KeyModifiers::NONE, KeyCode::Right) => move_selected(app, 0.02, 0.0).await,
                (KeyModifiers::NONE, KeyCode::Up) => move_selected(app, 0.0, -0.03).await,
                (KeyModifiers::NONE, KeyCode::Down) => move_selected(app, 0.0, 0.03).await,
                (KeyModifiers::SHIFT, KeyCode::Left) => {
                    app.center_x = (app.center_x - 0.05 / app.zoom).clamp(0.0, 1.0)
                }
                (KeyModifiers::SHIFT, KeyCode::Right) => {
                    app.center_x = (app.center_x + 0.05 / app.zoom).clamp(0.0, 1.0)
                }
                (KeyModifiers::SHIFT, KeyCode::Up) => {
                    app.center_y = (app.center_y - 0.05 / app.zoom).clamp(0.0, 1.0)
                }
                (KeyModifiers::SHIFT, KeyCode::Down) => {
                    app.center_y = (app.center_y + 0.05 / app.zoom).clamp(0.0, 1.0)
                }
                (KeyModifiers::NONE, KeyCode::Char('+') | KeyCode::Char('=')) => {
                    app.zoom = (app.zoom * 1.2).clamp(0.5, 6.0);
                    app.status = format!("zoom {:.1}x", app.zoom);
                }
                (KeyModifiers::NONE, KeyCode::Char('-')) => {
                    app.zoom = (app.zoom / 1.2).clamp(0.5, 6.0);
                    app.status = format!("zoom {:.1}x", app.zoom);
                }
                (KeyModifiers::NONE, KeyCode::Char('0')) => {
                    app.zoom = 1.0;
                    app.center_x = 0.5;
                    app.center_y = 0.5;
                    app.status = "viewport reset".into();
                }
                (_, KeyCode::Char('>')) => load_more(app, None, false),
                (KeyModifiers::NONE, KeyCode::Char('z')) => fit_selection(app, false),
                (KeyModifiers::SHIFT, KeyCode::Char('Z')) => fit_selection(app, true),
                (KeyModifiers::NONE, KeyCode::Char('f')) => cycle_dataset(app, false),
                (KeyModifiers::SHIFT, KeyCode::Char('F')) => cycle_type(app, false),
                (KeyModifiers::NONE, KeyCode::Char('a')) => auto_layout(app),
                (KeyModifiers::NONE, KeyCode::Char('p')) => toggle_pin(app).await,
                (KeyModifiers::NONE, KeyCode::Char(' ')) => toggle_mark(app),
                (KeyModifiers::NONE, KeyCode::Char('x')) => {
                    app.marked.clear();
                    app.status = "cleared marked units".into();
                }
                (KeyModifiers::NONE, KeyCode::Char('c')) => open_create(app),
                (KeyModifiers::NONE, KeyCode::Char('e')) => open_edit(app),
                (KeyModifiers::NONE, KeyCode::Char('d')) => begin_delete_facts(app),
                (KeyModifiers::NONE, KeyCode::Char('l')) => create_relation(app).await,
                (KeyModifiers::NONE, KeyCode::Char('t')) => {
                    app.relation_type = if app.relation_type == "relates_to" {
                        "proves".into()
                    } else {
                        "relates_to".into()
                    };
                    app.status = format!("relation type: {}", app.relation_type);
                }
                (KeyModifiers::NONE, KeyCode::Char('[')) => cycle_edge(app, -1),
                (KeyModifiers::NONE, KeyCode::Char(']')) => cycle_edge(app, 1),
                (KeyModifiers::NONE, KeyCode::Char('u')) => begin_delete_edge(app),
                _ => {}
            },
            Tab::Query => handle_query_normal_key(app, key),
            Tab::Analytics => {}
        },
    }
    false
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphConsoleAction {
    Continue,
    Exit,
    OpenCode {
        path: PathBuf,
        line: usize,
        column: usize,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphConsoleState {
    tab: String,
    dataset_filter: String,
    type_filter: String,
    center_x: f64,
    center_y: f64,
    zoom: f64,
    selected_fact_id: Option<String>,
    load_limit: usize,
}

impl Default for GraphConsoleState {
    fn default() -> Self {
        Self {
            tab: "graph".into(),
            dataset_filter: "all".into(),
            type_filter: "all".into(),
            center_x: 0.5,
            center_y: 0.5,
            zoom: 1.0,
            selected_fact_id: None,
            load_limit: GraphLoadOptions::default().fact_limit,
        }
    }
}

pub struct GraphConsole {
    app: App,
    initialized: bool,
    refresh_pending: bool,
    embedded: bool,
}

impl Default for GraphConsole {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphConsole {
    pub fn new() -> Self {
        Self {
            app: App::new(),
            initialized: false,
            refresh_pending: false,
            embedded: false,
        }
    }

    pub fn embedded() -> Self {
        let mut console = Self::new();
        console.embedded = true;
        console
    }

    pub async fn initialize(&mut self) {
        if self.initialized {
            return;
        }
        let view_note = match load_saved_views() {
            Ok(views) => {
                let count = views.len();
                self.app.saved_views = views;
                format!(" · {count} saved views")
            }
            Err(error) => format!(" · saved views unavailable: {error}"),
        };
        match refresh(&mut self.app).await {
            Ok(()) => {
                self.app.status = format!("connected · ns:agent · db:graph{view_note}");
            }
            Err(error) => self.app.status = format!("database unavailable: {error}"),
        }
        self.initialized = true;
    }

    pub fn render(&self, frame: &mut ratatui::Frame, area: Rect) {
        ui(frame, area, &self.app, self.embedded);
    }

    pub fn tick(&mut self) {
        drain_graph_jobs(&mut self.app);
        if self.refresh_pending && self.app.active_job.is_none() {
            self.refresh_pending = false;
            start_graph_job(&mut self.app, GraphJobKind::Refresh);
        }
    }

    pub fn request_refresh(&mut self) {
        if self.app.active_job.is_some() {
            self.refresh_pending = true;
        } else {
            start_graph_job(&mut self.app, GraphJobKind::Refresh);
        }
    }

    pub fn open_view(&mut self, view: &str) -> bool {
        let tab = match view.to_ascii_lowercase().as_str() {
            "graph" => Tab::Graph,
            "explorer" => Tab::Explorer,
            "query" => Tab::Query,
            "analytics" => Tab::Analytics,
            _ => return false,
        };
        self.app.tab = tab;
        self.app.mode = Mode::Normal;
        if tab == Tab::Query {
            self.app.query_focus = QueryFocus::Editor;
        }
        self.app.status = format!("{} view", tab.title());
        true
    }

    pub fn has_active_job(&self) -> bool {
        self.app.active_job.is_some()
    }

    pub fn node_counts(&self) -> (usize, u64) {
        (self.app.facts.len(), self.app.total_facts)
    }

    pub fn selected_fact_id(&self) -> Option<String> {
        self.app
            .facts
            .get(self.app.selected)
            .map(|fact| fact.id.clone())
    }

    pub fn state(&self) -> GraphConsoleState {
        GraphConsoleState {
            tab: self.app.tab.title().to_ascii_lowercase(),
            dataset_filter: self.app.dataset_filter.clone(),
            type_filter: self.app.type_filter.clone(),
            center_x: self.app.center_x,
            center_y: self.app.center_y,
            zoom: self.app.zoom,
            selected_fact_id: self
                .app
                .facts
                .get(self.app.selected)
                .map(|fact| fact.id.clone()),
            load_limit: self.app.load_limit,
        }
    }

    pub fn restore_state(&mut self, state: &GraphConsoleState) {
        self.app.tab = match state.tab.as_str() {
            "explorer" => Tab::Explorer,
            "query" => Tab::Query,
            "analytics" => Tab::Analytics,
            _ => Tab::Graph,
        };
        if state.dataset_filter == "all"
            || (valid_label(&state.dataset_filter)
                && (!self.initialized
                    || self
                        .app
                        .datasets
                        .iter()
                        .any(|dataset| dataset.name == state.dataset_filter)))
        {
            self.app.dataset_filter.clone_from(&state.dataset_filter);
        }
        if state.type_filter == "all"
            || (valid_label(&state.type_filter)
                && (!self.initialized
                    || self
                        .app
                        .facts
                        .iter()
                        .any(|fact| fact.fact_type == state.type_filter)))
        {
            self.app.type_filter.clone_from(&state.type_filter);
        }
        if state.center_x.is_finite() {
            self.app.center_x = state.center_x.clamp(0.0, 1.0);
        }
        if state.center_y.is_finite() {
            self.app.center_y = state.center_y.clamp(0.0, 1.0);
        }
        if state.zoom.is_finite() {
            self.app.zoom = state.zoom.clamp(0.5, 6.0);
        }
        self.app.load_limit = state.load_limit.clamp(1, 100_000);
        self.app.selected = state
            .selected_fact_id
            .as_ref()
            .and_then(|id| self.app.fact_lookup.get(id).copied())
            .filter(|index| fact_visible(&self.app, &self.app.facts[*index]))
            .or_else(|| visible_indices(&self.app).first().copied())
            .unwrap_or(0);
        self.app.edge_selected = 0;
    }

    pub async fn handle_event(&mut self, event: Event, area: Rect) -> GraphConsoleAction {
        match event {
            Event::Mouse(mouse) => handle_mouse(&mut self.app, mouse, area).await,
            Event::Resize(..) => {}
            Event::Key(key) => {
                if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
                    return GraphConsoleAction::Exit;
                }
                if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('g') {
                    cancel_graph_job(&mut self.app);
                    return GraphConsoleAction::Continue;
                }
                match self.app.mode {
                    Mode::Confirm => match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            execute_confirmation(&mut self.app).await
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                            self.app.confirm = None;
                            self.app.mode = Mode::Normal;
                            self.app.status = "change cancelled".into();
                        }
                        _ => {}
                    },
                    Mode::QueryEdit => match (key.modifiers, key.code) {
                        (_, KeyCode::Esc) => {
                            self.app.mode = Mode::Normal;
                            self.app.query_focus = QueryFocus::Editor;
                            self.app.status = "query editor focused".into();
                        }
                        (KeyModifiers::NONE, KeyCode::F(5))
                        | (KeyModifiers::CONTROL, KeyCode::Enter)
                        | (KeyModifiers::ALT, KeyCode::Enter) => {
                            execute_query_editor(&mut self.app, false);
                        }
                        (KeyModifiers::NONE, KeyCode::F(6)) => {
                            execute_query_editor(&mut self.app, true);
                        }
                        (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
                            self.app.query_editor = new_query_editor("");
                        }
                        _ => {
                            self.app.query_editor.input(key);
                        }
                    },
                    Mode::Create | Mode::Edit => match key.code {
                        KeyCode::Esc => {
                            self.app.mode = Mode::Normal;
                            self.app.status = "form cancelled".into();
                        }
                        KeyCode::Tab => self.app.form_field = (self.app.form_field + 1) % 5,
                        KeyCode::BackTab => self.app.form_field = (self.app.form_field + 4) % 5,
                        KeyCode::Enter => {
                            let editing = self.app.mode == Mode::Edit;
                            submit_form(&mut self.app, editing).await;
                        }
                        KeyCode::Backspace => {
                            self.app.form_values[self.app.form_field].pop();
                        }
                        KeyCode::Char(character)
                            if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            self.app.form_values[self.app.form_field].push(character);
                        }
                        _ => {}
                    },
                    Mode::Command | Mode::Search => match key.code {
                        KeyCode::Esc => {
                            self.app.mode = Mode::Normal;
                            self.app.command.clear();
                        }
                        KeyCode::Enter => {
                            let value = self.app.command.clone();
                            let command_mode = self.app.mode == Mode::Command;
                            self.app.command.clear();
                            self.app.mode = Mode::Normal;
                            if command_mode {
                                if execute_command(&mut self.app, &value).await {
                                    return GraphConsoleAction::Exit;
                                }
                            } else if !value.trim().is_empty() {
                                search(&mut self.app, &value);
                            }
                        }
                        KeyCode::Backspace => {
                            self.app.command.pop();
                        }
                        KeyCode::Char(character)
                            if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            self.app.command.push(character);
                        }
                        _ => {}
                    },
                    Mode::Normal => {
                        if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Char('o') {
                            if let Some(fact) = selected_fact(&self.app) {
                                if let Some(path) = &fact.code_path {
                                    return GraphConsoleAction::OpenCode {
                                        path: PathBuf::from(path),
                                        line: fact.code_start_line.unwrap_or(1),
                                        column: fact.code_column.unwrap_or(1),
                                    };
                                }
                                self.app.status = "selected unit has no code location".into();
                            }
                        }
                        if handle_normal_key(&mut self.app, key).await {
                            return GraphConsoleAction::Exit;
                        }
                    }
                }
            }
            _ => {}
        }
        GraphConsoleAction::Continue
    }
}

impl Drop for GraphConsole {
    fn drop(&mut self) {
        if let Some(active) = self.app.active_job.take() {
            active.abort.abort();
        }
    }
}

pub async fn run() -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    terminal.clear()?;
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
    let mut console = GraphConsole::new();
    console.initialize().await;
    let result = run_loop(&mut terminal, &mut console).await;
    crossterm::execute!(std::io::stdout(), DisableMouseCapture)?;
    crossterm::terminal::disable_raw_mode()?;
    ratatui::restore();
    result
}

async fn run_loop(
    terminal: &mut DefaultTerminal,
    console: &mut GraphConsole,
) -> anyhow::Result<()> {
    let mut events = EventStream::new();
    loop {
        console.tick();
        terminal.draw(|frame| console.render(frame, frame.area()))?;
        let redraw_delay = if console.has_active_job() { 100 } else { 250 };
        let event = tokio::select! {
            event = events.next() => match event {
                Some(event) => Some(event?),
                None => break,
            },
            _ = tokio::time::sleep(Duration::from_millis(redraw_delay)) => None,
        };
        let Some(event) = event else {
            continue;
        };
        let (width, height) = crossterm::terminal::size()?;
        match console
            .handle_event(event, Rect::new(0, 0, width, height))
            .await
        {
            GraphConsoleAction::Exit => break,
            GraphConsoleAction::OpenCode { path, line, .. } => {
                console.app.status = format!("code location: {}:{line}", path.display());
            }
            GraphConsoleAction::Continue => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(id: &str, fact_type: &str, dataset: &str, x: f64, y: f64) -> Fact {
        Fact {
            id: id.into(),
            fact_type: fact_type.into(),
            content: format!("content for {id}"),
            source: "test".into(),
            confidence: 1.0,
            tags: vec!["test".into()],
            dataset: dataset.into(),
            graph_x: x,
            graph_y: y,
            graph_pinned: false,
            code_path: None,
            code_start_line: None,
            code_end_line: None,
            code_column: None,
            code_symbol: None,
            run_id: None,
            timestamp: "now".into(),
        }
    }

    #[test]
    fn read_only_query_gate_checks_every_statement() {
        assert!(is_read_only_sql("SELECT * FROM fact; INFO FOR DB"));
        assert!(is_read_only_sql("RETURN true"));
        assert!(!is_read_only_sql("DELETE fact:test"));
        assert!(!is_read_only_sql(
            "SELECT * FROM fact; UPDATE fact:test SET content = 'x'"
        ));
    }

    #[test]
    fn query_focus_cycles_in_both_directions() {
        let focus = QueryFocus::Templates;
        assert_eq!(focus.next(false), QueryFocus::History);
        assert_eq!(focus.next(false).next(false), QueryFocus::Editor);
        assert_eq!(focus.next(true), QueryFocus::Results);
        assert_eq!(focus.next(true).next(true), QueryFocus::Editor);
    }

    #[test]
    fn query_template_opens_in_the_multiline_editor() {
        let mut app = App::new();
        begin_query(&mut app, "SELECT *\nFROM fact\nLIMIT 5");
        assert!(app.mode == Mode::QueryEdit);
        assert_eq!(app.query_focus, QueryFocus::Editor);
        assert_eq!(
            app.query_editor.lines(),
            ["SELECT *", "FROM fact", "LIMIT 5"]
        );
    }

    #[test]
    fn query_editor_requires_explicit_confirmation_for_mutations() {
        let mut app = App::new();
        app.query_editor = new_query_editor("DELETE fact:test");
        app.mode = Mode::QueryEdit;
        execute_query_editor(&mut app, false);
        assert!(app.mode == Mode::QueryEdit);
        assert!(app.active_job.is_none());
        assert!(app.status.contains("press F6"));
    }

    #[test]
    fn filters_apply_dataset_and_type_together() {
        let mut app = App::new();
        app.facts = vec![
            fact("fact:a", "finding", "alpha", 0.2, 0.3),
            fact("fact:b", "decision", "alpha", 0.8, 0.7),
            fact("fact:c", "finding", "beta", 0.5, 0.5),
        ];
        app.dataset_filter = "alpha".into();
        app.type_filter = "finding".into();
        assert_eq!(visible_indices(&app), vec![0]);
    }

    #[test]
    fn graph_metrics_detect_orphans_and_duplicate_edges() {
        let mut app = App::new();
        app.facts = vec![
            fact("fact:a", "finding", "default", 0.2, 0.3),
            fact("fact:b", "decision", "default", 0.8, 0.7),
            fact("fact:c", "memory", "default", 0.5, 0.5),
        ];
        app.edges = vec![
            Edge {
                id: "relates_to:one".into(),
                relation_type: "relates_to".into(),
                from_id: "fact:a".into(),
                to_id: "fact:b".into(),
            },
            Edge {
                id: "relates_to:two".into(),
                relation_type: "relates_to".into(),
                from_id: "fact:a".into(),
                to_id: "fact:b".into(),
            },
        ];
        let (degrees, orphans, duplicates, pinned) = graph_metrics(&app);
        assert_eq!(degrees["fact:a"], 2);
        assert_eq!(orphans, 1);
        assert_eq!(duplicates, 1);
        assert_eq!(pinned, 0);
    }

    #[test]
    fn viewport_mapping_respects_zoom_and_center() {
        let mut app = App::new();
        let canvas = Rect::new(10, 5, 21, 11);
        let center = fact("fact:a", "finding", "default", 0.5, 0.5);
        assert_eq!(screen_position(&app, &center, canvas), Some((20, 10)));
        app.zoom = 2.0;
        let edge = fact("fact:b", "finding", "default", 0.75, 0.5);
        assert_eq!(screen_position(&app, &edge, canvas), Some((30, 10)));
        let hidden = fact("fact:c", "finding", "default", 1.0, 0.5);
        assert_eq!(screen_position(&app, &hidden, canvas), None);
    }

    #[test]
    fn form_validation_rejects_injection_and_bad_confidence() {
        let mut app = App::new();
        app.form_values = [
            "finding; DELETE fact".into(),
            "content".into(),
            "default".into(),
            "safe-tag".into(),
            "0.8".into(),
        ];
        assert!(parse_form(&app).is_err());
        app.form_values[0] = "finding".into();
        app.form_values[4] = "1.5".into();
        assert!(parse_form(&app).is_err());
    }

    #[test]
    fn snapshot_import_validation_rejects_dangling_edges() {
        let path = std::env::temp_dir().join(format!(
            "uintell-snapshot-test-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let mut snapshot = ExportSnapshot {
            version: 1,
            namespace: "agent".into(),
            database: "graph".into(),
            exported_at_unix: 0,
            total_facts: 1,
            partial: false,
            datasets: vec![Dataset {
                name: "default".into(),
                description: String::new(),
            }],
            facts: vec![fact("fact:a", "finding", "default", 0.2, 0.3)],
            edges: Vec::new(),
        };
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert!(load_import_snapshot(path.to_str().unwrap()).is_ok());

        snapshot.edges.push(Edge {
            id: "relates_to:test".into(),
            relation_type: "relates_to".into(),
            from_id: "fact:a".into(),
            to_id: "fact:missing".into(),
        });
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert!(load_import_snapshot(path.to_str().unwrap()).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn virtual_window_tracks_selection_without_allocating_every_row() {
        assert_eq!(virtual_window(10_000, 5_000, 20), (4_990, 5_010));
        assert_eq!(virtual_window(8, 7, 20), (0, 8));
        assert_eq!(virtual_window(0, 0, 20), (0, 0));
    }

    #[test]
    fn lasso_marks_only_nodes_inside_the_screen_rectangle() {
        let mut app = App::new();
        app.facts = vec![
            fact("fact:a", "finding", "default", 0.2, 0.2),
            fact("fact:b", "finding", "default", 0.5, 0.5),
            fact("fact:c", "finding", "default", 0.8, 0.8),
        ];
        rebuild_graph_caches(&mut app);
        let canvas = Rect::new(10, 5, 101, 101);
        app.lasso_start = Some((20, 15));
        app.lasso_end = Some((60, 55));
        finish_lasso(&mut app, canvas);
        assert!(app.marked.contains("fact:a"));
        assert!(app.marked.contains("fact:b"));
        assert!(!app.marked.contains("fact:c"));
    }

    #[test]
    fn fit_selection_frames_marked_nodes() {
        let mut app = App::new();
        app.facts = vec![
            fact("fact:a", "finding", "default", 0.2, 0.3),
            fact("fact:b", "finding", "default", 0.8, 0.7),
        ];
        rebuild_graph_caches(&mut app);
        app.marked.insert("fact:a".into());
        app.marked.insert("fact:b".into());
        fit_selection(&mut app, false);
        assert!((app.center_x - 0.5).abs() < f64::EPSILON);
        assert!((app.center_y - 0.5).abs() < f64::EPSILON);
        assert!(app.zoom > 1.0);
    }

    #[test]
    fn mutating_query_requires_an_explicit_confirmation_step() {
        let mut app = App::new();
        execute_query(&mut app, "DELETE fact:test", true);
        assert!(app.mode == Mode::Confirm);
        assert!(matches!(
            app.confirm,
            Some(ConfirmAction::Query(ref sql)) if sql == "DELETE fact:test"
        ));
    }

    #[test]
    fn saved_view_validation_rejects_unsafe_or_invalid_coordinates() {
        let valid = SavedView {
            name: "investigation-1".into(),
            dataset_filter: "default".into(),
            type_filter: "all".into(),
            center_x: 0.5,
            center_y: 0.5,
            zoom: 2.0,
        };
        assert!(valid_saved_view(&valid));
        let mut invalid = valid.clone();
        invalid.name = "bad; DELETE fact".into();
        assert!(!valid_saved_view(&invalid));
        invalid = valid;
        invalid.center_x = f64::NAN;
        assert!(!valid_saved_view(&invalid));
    }

    #[tokio::test]
    async fn background_job_cancellation_aborts_immediately() {
        let mut app = App::new();
        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        app.active_job = Some(ActiveGraphJob {
            id: 1,
            kind: GraphJobKind::Refresh,
            started: Instant::now(),
            percent: 10,
            phase: "test",
            abort: handle.abort_handle(),
        });

        cancel_graph_job(&mut app);

        let result = tokio::time::timeout(Duration::from_millis(250), handle)
            .await
            .expect("cancelled task should stop promptly")
            .expect_err("cancelled task should return a join error");
        assert!(result.is_cancelled());
        assert!(app.active_job.is_none());
        assert_eq!(app.status, "cancelled refresh");
    }

    #[tokio::test]
    async fn embedded_console_routes_its_own_views_and_exit() {
        let mut console = GraphConsole::embedded();
        let area = Rect::new(0, 3, 100, 28);

        let action = console
            .handle_event(
                Event::Key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)),
                area,
            )
            .await;
        assert_eq!(action, GraphConsoleAction::Continue);
        assert!(console.app.tab == Tab::Explorer);

        let action = console
            .handle_event(
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
                area,
            )
            .await;
        assert_eq!(action, GraphConsoleAction::Exit);
    }

    #[tokio::test]
    async fn embedded_console_opens_code_locations_in_the_parent_tui() {
        let mut console = GraphConsole::embedded();
        let mut location = fact("fact:code", "code_location", "code", 0.4, 0.4);
        location.code_path = Some("/tmp/example.rs".into());
        location.code_start_line = Some(12);
        location.code_column = Some(5);
        console.app.facts = vec![location];
        rebuild_graph_caches(&mut console.app);

        let action = console
            .handle_event(
                Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)),
                Rect::new(0, 3, 100, 28),
            )
            .await;

        assert_eq!(
            action,
            GraphConsoleAction::OpenCode {
                path: PathBuf::from("/tmp/example.rs"),
                line: 12,
                column: 5,
            }
        );
    }

    #[test]
    fn graph_console_opens_named_views_for_global_navigation() {
        let mut console = GraphConsole::embedded();
        assert!(console.open_view("query"));
        assert!(console.app.tab == Tab::Query);
        assert_eq!(console.app.query_focus, QueryFocus::Editor);
        assert!(console.open_view("analytics"));
        assert!(console.app.tab == Tab::Analytics);
        assert!(!console.open_view("unknown"));
    }

    #[test]
    fn embedded_console_renders_only_inside_parent_area() {
        use ratatui::{backend::TestBackend, Terminal};

        let console = GraphConsole::embedded();
        let mut terminal = Terminal::new(TestBackend::new(100, 35)).unwrap();
        let area = Rect::new(5, 4, 80, 25);
        terminal.draw(|frame| console.render(frame, area)).unwrap();
        let buffer = terminal.backend().buffer();

        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                if (area.x..area.right()).contains(&x) && (area.y..area.bottom()).contains(&y) {
                    continue;
                }
                assert_eq!(buffer[(x, y)].symbol(), " ");
            }
        }
    }

    #[test]
    fn graph_console_state_restores_view_filters_and_selection() {
        let mut console = GraphConsole::embedded();
        console.app.facts = vec![
            fact("fact:a", "finding", "default", 0.2, 0.3),
            fact("fact:b", "memory", "default", 0.7, 0.8),
        ];
        rebuild_graph_caches(&mut console.app);
        console.app.tab = Tab::Explorer;
        console.app.type_filter = "memory".into();
        console.app.selected = 1;
        console.app.center_x = 0.7;
        console.app.zoom = 2.5;
        let encoded = serde_json::to_vec(&console.state()).unwrap();
        let state: GraphConsoleState = serde_json::from_slice(&encoded).unwrap();

        let mut restored = GraphConsole::embedded();
        restored.app.facts = console.app.facts.clone();
        restored.app.datasets = console.app.datasets.clone();
        rebuild_graph_caches(&mut restored.app);
        restored.initialized = true;
        restored.restore_state(&state);

        assert!(restored.app.tab == Tab::Explorer);
        assert_eq!(restored.app.type_filter, "memory");
        assert_eq!(restored.app.facts[restored.app.selected].id, "fact:b");
        assert!((restored.app.center_x - 0.7).abs() < f64::EPSILON);
        assert!((restored.app.zoom - 2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn unicode_truncation_is_safe() {
        assert_eq!(trunc("αβγδε", 3), "αβγ...");
        assert_eq!(trunc("αβ", 3), "αβ");
    }
}
