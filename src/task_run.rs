//! Durable autonomous coding runs built on Rig's serializable AgentRun.

use crate::confirm::ConfirmState;
use crate::permissions::{self, PermissionResult};
use anyhow::{Context, Result};
use rig_core::agent::run::{AgentRun, AgentRunStep, ModelTurn, ModelTurnOutcome};
use rig_core::agent::{Agent, InvalidToolCallHookAction};
use rig_core::completion::{Completion, CompletionModel};
use rig_core::message::{ToolResultContent, UserContent};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};

const TASK_RUN_VERSION: u32 = 1;
const MAX_RUN_BYTES: u64 = 32 * 1024 * 1024;
const MAX_OBJECTIVE_CHARS: usize = 32_000;
const MAX_STORED_OUTPUT_CHARS: usize = 120_000;
const MAX_CONTEXT_OUTPUT_CHARS: usize = 20_000;
const MAX_CONTEXT_CHARS: usize = 100_000;
const MAX_EVENT_DETAIL_CHARS: usize = 8_000;
const MAX_EVENTS: usize = 1_000;
const MAX_LISTED_RUNS: usize = 200;
const TASK_MAX_TURNS: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunStatus {
    Pending,
    Running,
    Paused,
    Completed,
    NeedsAttention,
    Failed,
    Cancelled,
}

impl TaskRunStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::NeedsAttention => "needs attention",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::NeedsAttention)
    }

    pub fn is_resumable(self) -> bool {
        !self.is_terminal()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStepStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStepStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStepKind {
    Plan,
    Implement,
    Verify,
    Review,
    Repair,
    Finalize,
}

impl TaskStepKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Plan => "Inspect and plan",
            Self::Implement => "Implement",
            Self::Verify => "Verify",
            Self::Review => "Review",
            Self::Repair => "Repair",
            Self::Finalize => "Finalize",
        }
    }

    fn tool_allowed(self, tool_name: &str, memory_writes: bool) -> bool {
        if !memory_writes && matches!(tool_name, "graph_store" | "graph_edit" | "graph_forget") {
            return false;
        }
        match self {
            Self::Plan => matches!(
                tool_name,
                "file_read"
                    | "file_search"
                    | "graph_context"
                    | "graph_query"
                    | "browser"
                    | "web_search"
            ),
            Self::Implement | Self::Repair => true,
            Self::Verify | Self::Review => matches!(
                tool_name,
                "file_read"
                    | "file_search"
                    | "terminal"
                    | "code_exec"
                    | "graph_context"
                    | "graph_query"
                    | "browser"
                    | "web_search"
            ),
            Self::Finalize => {
                tool_name == "graph_context" || (memory_writes && tool_name == "graph_store")
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskStep {
    pub kind: TaskStepKind,
    pub title: String,
    pub status: TaskStepStatus,
    pub attempt: u32,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub output: Option<String>,
    pub error: Option<String>,
}

impl TaskStep {
    fn new(kind: TaskStepKind) -> Self {
        Self {
            kind,
            title: kind.title().into(),
            status: TaskStepStatus::Pending,
            attempt: 0,
            started_at: None,
            finished_at: None,
            output: None,
            error: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskEventKind {
    Created,
    Started,
    Resumed,
    StepStarted,
    ModelCall,
    ToolCall,
    ToolResult,
    StepCompleted,
    RepairScheduled,
    Completed,
    Warning,
    Failed,
    Cancelled,
}

impl TaskEventKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Started => "started",
            Self::Resumed => "resumed",
            Self::StepStarted => "step",
            Self::ModelCall => "model",
            Self::ToolCall => "tool",
            Self::ToolResult => "result",
            Self::StepCompleted => "done",
            Self::RepairScheduled => "repair",
            Self::Completed => "complete",
            Self::Warning => "warning",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskEvent {
    pub sequence: u64,
    pub timestamp: u64,
    pub kind: TaskEventKind,
    pub step: Option<usize>,
    pub title: String,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TaskCheckpoint {
    agent_run: AgentRun,
    #[serde(default)]
    tool_results: BTreeMap<String, UserContent>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRun {
    version: u32,
    pub id: String,
    pub objective: String,
    pub workspace: PathBuf,
    pub provider: String,
    #[serde(default)]
    pub memory_writes: bool,
    pub status: TaskRunStatus,
    pub steps: Vec<TaskStep>,
    pub current_step: usize,
    pub repair_rounds: u32,
    pub max_repair_rounds: u32,
    pub quality_gate_failed: bool,
    pub created_at: u64,
    pub updated_at: u64,
    pub finished_at: Option<u64>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub events: Vec<TaskEvent>,
    event_sequence: u64,
    checkpoint: Option<TaskCheckpoint>,
}

impl TaskRun {
    fn new(objective: String, workspace: PathBuf, provider: String) -> Self {
        let now = unix_millis();
        let id = format!("run-{now}-{:08x}", rand::random::<u32>());
        let mut run = Self {
            version: TASK_RUN_VERSION,
            id,
            objective,
            workspace,
            provider,
            memory_writes: false,
            status: TaskRunStatus::Pending,
            steps: [
                TaskStepKind::Plan,
                TaskStepKind::Implement,
                TaskStepKind::Verify,
                TaskStepKind::Review,
                TaskStepKind::Finalize,
            ]
            .into_iter()
            .map(TaskStep::new)
            .collect(),
            current_step: 0,
            repair_rounds: 0,
            max_repair_rounds: 2,
            quality_gate_failed: false,
            created_at: now,
            updated_at: now,
            finished_at: None,
            result: None,
            error: None,
            events: Vec::new(),
            event_sequence: 0,
            checkpoint: None,
        };
        run.record_event(
            TaskEventKind::Created,
            None,
            "Task run created",
            run.objective.clone(),
        );
        run
    }

    fn record_event(
        &mut self,
        kind: TaskEventKind,
        step: Option<usize>,
        title: impl Into<String>,
        detail: impl AsRef<str>,
    ) {
        self.event_sequence += 1;
        self.updated_at = unix_millis();
        self.events.push(TaskEvent {
            sequence: self.event_sequence,
            timestamp: self.updated_at,
            kind,
            step,
            title: title.into(),
            detail: truncate_chars(detail.as_ref(), MAX_EVENT_DETAIL_CHARS),
        });
        if self.events.len() > MAX_EVENTS {
            self.events.drain(..self.events.len() - MAX_EVENTS);
        }
    }

    fn complete_step(&mut self, output: String) {
        let index = self.current_step;
        let kind = self.steps[index].kind;
        let output = truncate_chars(&output, MAX_STORED_OUTPUT_CHARS);
        {
            let step = &mut self.steps[index];
            step.status = TaskStepStatus::Completed;
            step.finished_at = Some(unix_millis());
            step.output = Some(output.clone());
            step.error = None;
        }
        self.checkpoint = None;
        self.record_event(
            TaskEventKind::StepCompleted,
            Some(index),
            format!("{} completed", kind.title()),
            preview(&output, 500),
        );

        match kind {
            TaskStepKind::Verify if final_verdict(&output) != Some("PASS") => {
                self.schedule_repair_cycle(
                    index,
                    false,
                    "Verification did not pass",
                    "Verification failed or omitted its required PASS verdict.",
                );
            }
            TaskStepKind::Review if final_verdict(&output) != Some("APPROVED") => {
                self.schedule_repair_cycle(
                    index,
                    true,
                    "Review did not approve the implementation",
                    "Review requested changes or omitted its required APPROVED verdict.",
                );
            }
            _ => {}
        }
        self.current_step += 1;
    }

    fn schedule_repair_cycle(
        &mut self,
        index: usize,
        include_review: bool,
        title: &str,
        detail: &str,
    ) {
        if self.repair_rounds < self.max_repair_rounds {
            self.repair_rounds += 1;
            let mut inserted = vec![
                TaskStep::new(TaskStepKind::Repair),
                TaskStep::new(TaskStepKind::Verify),
            ];
            if include_review {
                inserted.push(TaskStep::new(TaskStepKind::Review));
            }
            self.steps.splice(index + 1..index + 1, inserted);
            self.record_event(
                TaskEventKind::RepairScheduled,
                Some(index),
                format!("Repair round {} scheduled", self.repair_rounds),
                detail,
            );
        } else {
            self.quality_gate_failed = true;
            self.record_event(
                TaskEventKind::Warning,
                Some(index),
                title,
                "The repair budget is exhausted; finalization must report the unresolved quality gate.",
            );
        }
    }

    fn context_outputs(&self) -> String {
        let sections = self
            .steps
            .iter()
            .take(self.current_step)
            .filter_map(|step| {
                step.output.as_ref().map(|output| {
                    format!(
                        "## {}\n{}",
                        step.title,
                        truncate_chars(output, MAX_CONTEXT_OUTPUT_CHARS)
                    )
                })
            })
            .collect::<Vec<_>>();
        let mut remaining = MAX_CONTEXT_CHARS;
        let mut retained = Vec::new();
        for section in sections.into_iter().rev() {
            if remaining == 0 {
                break;
            }
            let section = truncate_chars(&section, remaining);
            remaining = remaining.saturating_sub(section.chars().count());
            retained.push(section);
        }
        retained.reverse();
        retained.join("\n\n")
    }

    pub fn view(&self) -> TaskView {
        TaskView {
            id: self.id.clone(),
            objective: self.objective.clone(),
            workspace: self.workspace.clone(),
            provider: self.provider.clone(),
            memory_writes: self.memory_writes,
            status: self.status,
            steps: self.steps.clone(),
            current_step: self.current_step,
            repair_rounds: self.repair_rounds,
            created_at: self.created_at,
            updated_at: self.updated_at,
            finished_at: self.finished_at,
            result: self.result.clone(),
            error: self.error.clone(),
            events: self.events.clone(),
        }
    }

    pub fn summary(&self) -> TaskRunSummary {
        TaskRunSummary {
            id: self.id.clone(),
            objective: preview(&self.objective, 120),
            workspace: self.workspace.clone(),
            provider: self.provider.clone(),
            status: self.status,
            current_step: self.current_step,
            total_steps: self.steps.len(),
            updated_at: self.updated_at,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TaskView {
    pub id: String,
    pub objective: String,
    pub workspace: PathBuf,
    pub provider: String,
    pub memory_writes: bool,
    pub status: TaskRunStatus,
    pub steps: Vec<TaskStep>,
    pub current_step: usize,
    pub repair_rounds: u32,
    pub created_at: u64,
    pub updated_at: u64,
    pub finished_at: Option<u64>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub events: Vec<TaskEvent>,
}

#[derive(Clone, Debug)]
pub struct TaskRunSummary {
    pub id: String,
    pub objective: String,
    pub workspace: PathBuf,
    pub provider: String,
    pub status: TaskRunStatus,
    pub current_step: usize,
    pub total_steps: usize,
    pub updated_at: u64,
}

#[derive(Clone, Debug)]
pub enum TaskNotification {
    Updated(TaskView),
    DriverError { id: String, error: String },
}

#[derive(Clone, Debug)]
pub struct TaskStore {
    root: PathBuf,
}

impl Default for TaskStore {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        Self::at(PathBuf::from(home).join(".uintell").join("runs"))
    }
}

impl TaskStore {
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn create(
        &self,
        objective: impl Into<String>,
        workspace: &Path,
        provider: impl Into<String>,
    ) -> Result<TaskRun> {
        self.create_with_memory(objective, workspace, provider, false)
    }

    pub fn create_with_memory(
        &self,
        objective: impl Into<String>,
        workspace: &Path,
        provider: impl Into<String>,
        memory_writes: bool,
    ) -> Result<TaskRun> {
        let objective = objective.into();
        validate_objective(&objective)?;
        let workspace = workspace
            .canonicalize()
            .with_context(|| format!("resolve workspace {}", workspace.display()))?;
        if !workspace.is_dir() {
            anyhow::bail!("task workspace is not a directory: {}", workspace.display());
        }
        let mut run = TaskRun::new(objective, workspace, provider.into());
        run.memory_writes = memory_writes;
        self.save(&run)?;
        Ok(run)
    }

    pub fn save(&self, run: &TaskRun) -> Result<()> {
        validate_run(run)?;
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create task store {}", self.root.display()))?;
        let path = self.run_path(&run.id)?;
        let bytes = serde_json::to_vec_pretty(run).context("serialize task run")?;
        if bytes.len() as u64 > MAX_RUN_BYTES {
            anyhow::bail!(
                "task checkpoint exceeds {} MiB",
                MAX_RUN_BYTES / 1024 / 1024
            );
        }
        atomic_private_write(&path, &bytes).with_context(|| format!("persist task run {}", run.id))
    }

    pub fn load(&self, id: &str) -> Result<TaskRun> {
        let path = self.run_path(id)?;
        let metadata =
            std::fs::metadata(&path).with_context(|| format!("inspect task run {id}"))?;
        if metadata.len() > MAX_RUN_BYTES {
            anyhow::bail!(
                "task checkpoint exceeds {} MiB",
                MAX_RUN_BYTES / 1024 / 1024
            );
        }
        let mut run: TaskRun = serde_json::from_slice(
            &std::fs::read(&path).with_context(|| format!("read task run {id}"))?,
        )
        .with_context(|| format!("parse task run {id}"))?;
        validate_run(&run)?;
        if run.status == TaskRunStatus::Running
            && !lock_owner_is_alive(&self.root.join(format!("{id}.lock")))
        {
            run.status = TaskRunStatus::Paused;
            if let Some(step) = run.steps.get_mut(run.current_step) {
                if step.status == TaskStepStatus::Running {
                    step.status = TaskStepStatus::Pending;
                }
            }
        }
        if run.status.is_terminal() {
            let finalizer_output = run
                .steps
                .iter()
                .rev()
                .find(|step| step.kind == TaskStepKind::Finalize)
                .and_then(|step| step.output.clone());
            run.result = Some(build_final_report(&run, finalizer_output.as_deref()));
        }
        Ok(run)
    }

    pub fn list(&self) -> Result<Vec<TaskRunSummary>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut paths = std::fs::read_dir(&self.root)
            .with_context(|| format!("read task store {}", self.root.display()))?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension().and_then(|value| value.to_str()) == Some("json")).then_some(path)
            })
            .collect::<Vec<_>>();
        paths.sort_by_key(|path| {
            std::cmp::Reverse(
                path.metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(UNIX_EPOCH),
            )
        });
        let mut summaries = Vec::new();
        for path in paths.into_iter().take(MAX_LISTED_RUNS) {
            let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            if let Ok(run) = self.load(id) {
                summaries.push(run.summary());
            }
        }
        Ok(summaries)
    }

    fn run_path(&self, id: &str) -> Result<PathBuf> {
        validate_run_id(id)?;
        Ok(self.root.join(format!("{id}.json")))
    }

    fn acquire(&self, id: &str) -> Result<TaskRunLease> {
        validate_run_id(id)?;
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create task store {}", self.root.display()))?;
        let path = self.root.join(format!("{id}.lock"));
        for attempt in 0..2 {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())?;
                    file.sync_all()?;
                    return Ok(TaskRunLease { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                    if lock_owner_is_alive(&path) {
                        anyhow::bail!("task run {id} is already active in another process");
                    }
                    std::fs::remove_file(&path)
                        .with_context(|| format!("remove stale task lock {}", path.display()))?;
                }
                Err(error) => return Err(error).context("acquire task-run lease"),
            }
        }
        anyhow::bail!("could not acquire task-run lease for {id}")
    }
}

struct TaskRunLease {
    path: PathBuf,
}

impl Drop for TaskRunLease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub async fn execute<M>(
    store: TaskStore,
    mut run: TaskRun,
    agent: Arc<Agent<M>>,
    approvals: Arc<ConfirmState>,
    updates: Option<mpsc::UnboundedSender<TaskNotification>>,
    mut cancel: watch::Receiver<bool>,
) -> Result<TaskRun>
where
    M: CompletionModel + 'static,
{
    let _lease = store.acquire(&run.id)?;
    if run.status.is_terminal() {
        anyhow::bail!("task run {} is already {}", run.id, run.status.label());
    }
    let current_workspace = std::env::current_dir()
        .context("read current workspace")?
        .canonicalize()
        .context("resolve current workspace")?;
    if current_workspace != run.workspace {
        anyhow::bail!(
            "task run belongs to {}, but the process is in {}",
            run.workspace.display(),
            current_workspace.display()
        );
    }

    let execution = execute_inner(
        &store,
        &mut run,
        agent,
        approvals,
        updates.as_ref(),
        &mut cancel,
    )
    .await;
    if let Err(error) = execution {
        run.status = TaskRunStatus::Failed;
        run.error = Some(error.to_string());
        if let Some(step) = run.steps.get_mut(run.current_step) {
            step.status = TaskStepStatus::Failed;
            step.finished_at = Some(unix_millis());
            step.error = Some(error.to_string());
        }
        run.record_event(
            TaskEventKind::Failed,
            Some(run.current_step),
            "Task run failed",
            error.to_string(),
        );
        let _ = save_and_publish(&store, &run, updates.as_ref());
        return Err(error);
    }
    Ok(run)
}

async fn execute_inner<M>(
    store: &TaskStore,
    run: &mut TaskRun,
    agent: Arc<Agent<M>>,
    approvals: Arc<ConfirmState>,
    updates: Option<&mpsc::UnboundedSender<TaskNotification>>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<()>
where
    M: CompletionModel + 'static,
{
    let resumed = run.status != TaskRunStatus::Pending;
    run.status = TaskRunStatus::Running;
    run.error = None;
    run.record_event(
        if resumed {
            TaskEventKind::Resumed
        } else {
            TaskEventKind::Started
        },
        Some(run.current_step),
        if resumed {
            "Task run resumed"
        } else {
            "Task run started"
        },
        format!("workspace: {}", run.workspace.display()),
    );
    save_and_publish(store, run, updates)?;

    if let Err(error) = crate::session::restart_session().await {
        run.record_event(
            TaskEventKind::Warning,
            Some(run.current_step),
            "Terminal session reset failed",
            error.to_string(),
        );
        save_and_publish(store, run, updates)?;
    }

    while run.current_step < run.steps.len() {
        if cancellation_is_set(cancel) {
            cancel_run(store, run, updates)?;
            return Ok(());
        }
        if run.steps[run.current_step].status == TaskStepStatus::Completed {
            run.current_step += 1;
            continue;
        }

        let step_index = run.current_step;
        let step_kind = run.steps[step_index].kind;
        if run.checkpoint.is_none() {
            let prompt = build_step_prompt(run, step_kind);
            run.checkpoint = Some(TaskCheckpoint {
                agent_run: AgentRun::new(prompt)
                    .max_turns(TASK_MAX_TURNS)
                    .max_invalid_tool_call_retries(2),
                tool_results: BTreeMap::new(),
            });
            let step = &mut run.steps[step_index];
            step.status = TaskStepStatus::Running;
            step.attempt += 1;
            step.started_at.get_or_insert_with(unix_millis);
            step.finished_at = None;
            step.error = None;
            run.record_event(
                TaskEventKind::StepStarted,
                Some(step_index),
                format!("{} started", step_kind.title()),
                format!("attempt {}", run.steps[step_index].attempt),
            );
            save_and_publish(store, run, updates)?;
        } else {
            run.steps[step_index].status = TaskStepStatus::Running;
        }

        drive_step(
            store,
            run,
            agent.clone(),
            approvals.clone(),
            updates,
            cancel,
        )
        .await?;
        if run.status == TaskRunStatus::Cancelled {
            return Ok(());
        }
    }

    run.finished_at = Some(unix_millis());
    run.status = if run.quality_gate_failed {
        TaskRunStatus::NeedsAttention
    } else {
        TaskRunStatus::Completed
    };
    let finalizer_output = run
        .steps
        .iter()
        .rev()
        .find(|step| step.kind == TaskStepKind::Finalize)
        .and_then(|step| step.output.clone());
    run.result = Some(build_final_report(run, finalizer_output.as_deref()));
    run.record_event(
        TaskEventKind::Completed,
        None,
        format!("Task run {}", run.status.label()),
        run.result
            .as_deref()
            .map(|result| preview(result, 1_000))
            .unwrap_or_else(|| "Run finished without a final report.".into()),
    );
    save_and_publish(store, run, updates)
}

async fn drive_step<M>(
    store: &TaskStore,
    run: &mut TaskRun,
    agent: Arc<Agent<M>>,
    approvals: Arc<ConfirmState>,
    updates: Option<&mpsc::UnboundedSender<TaskNotification>>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<()>
where
    M: CompletionModel + 'static,
{
    let step_index = run.current_step;
    let step_kind = run.steps[step_index].kind;
    let checkpoint = run
        .checkpoint
        .clone()
        .context("active task step lost its checkpoint")?;
    let mut machine = checkpoint.agent_run;
    let mut completed_tools = checkpoint.tool_results;

    loop {
        let safe_machine = machine.clone();
        let action = match machine.next_step() {
            Ok(action) => action,
            Err(error) => {
                save_checkpoint(store, run, safe_machine, completed_tools, updates)?;
                return Err(anyhow::Error::from(error));
            }
        };
        match action {
            AgentRunStep::CallModel {
                prompt,
                history,
                turn,
            } => {
                run.record_event(
                    TaskEventKind::ModelCall,
                    Some(step_index),
                    format!("Model call {turn}"),
                    step_kind.title(),
                );
                save_checkpoint(
                    store,
                    run,
                    safe_machine.clone(),
                    completed_tools.clone(),
                    updates,
                )?;

                let response = tokio::select! {
                    _ = wait_for_cancellation(cancel) => {
                        save_checkpoint(store, run, safe_machine, completed_tools, updates)?;
                        cancel_run(store, run, updates)?;
                        return Ok(());
                    }
                    response = async {
                        let request = agent.completion(prompt, history).await?;
                        Ok::<_, anyhow::Error>(request.send().await?)
                    } => response.context("model call failed")?,
                };

                let tool_names: BTreeSet<String> = agent
                    .tool_server_handle
                    .get_tool_defs(None)
                    .await
                    .context("load task tool definitions")?
                    .into_iter()
                    .map(|definition| definition.name)
                    .collect();
                let mut outcome = machine
                    .model_response(ModelTurn::new(
                        response.message_id,
                        response.choice,
                        response.usage,
                        tool_names.clone(),
                        tool_names,
                    ))
                    .map_err(anyhow::Error::from)?;
                while let ModelTurnOutcome::NeedsResolution(context) = outcome {
                    outcome = machine
                        .resolve_invalid_tool_call(InvalidToolCallHookAction::retry(format!(
                            "`{}` is unavailable. Use one of the advertised tools or continue without it.",
                            context.tool_name
                        )))
                        .map_err(anyhow::Error::from)?;
                }
                completed_tools.clear();
                save_checkpoint(
                    store,
                    run,
                    machine.clone(),
                    completed_tools.clone(),
                    updates,
                )?;
            }
            AgentRunStep::CallTools { calls } => {
                save_checkpoint(
                    store,
                    run,
                    machine.clone(),
                    completed_tools.clone(),
                    updates,
                )?;
                for call in &calls {
                    let call_id = call.tool_call.id.clone();
                    if completed_tools.contains_key(&call_id) {
                        continue;
                    }
                    if let Some(result) = &call.preresolved_result {
                        completed_tools.insert(call_id, result.clone());
                        continue;
                    }

                    let tool_name = call.tool_call.function.name.clone();
                    let args = call.tool_call.function.arguments.to_string();
                    run.record_event(
                        TaskEventKind::ToolCall,
                        Some(step_index),
                        format!("Tool: {tool_name}"),
                        preview(&args, 1_000),
                    );
                    save_checkpoint(
                        store,
                        run,
                        machine.clone(),
                        completed_tools.clone(),
                        updates,
                    )?;

                    let output = if !step_kind.tool_allowed(&tool_name, run.memory_writes) {
                        format!(
                            "TOOL BLOCKED: `{tool_name}` is not allowed during the {} phase. Continue within the phase constraints.",
                            step_kind.title()
                        )
                    } else {
                        execute_task_tool(&agent, &approvals, &tool_name, &args, cancel).await?
                    };
                    if cancellation_is_set(cancel) {
                        save_checkpoint(store, run, machine, completed_tools, updates)?;
                        cancel_run(store, run, updates)?;
                        return Ok(());
                    }
                    run.record_event(
                        TaskEventKind::ToolResult,
                        Some(step_index),
                        format!("Result: {tool_name}"),
                        preview(&output, 1_500),
                    );
                    completed_tools.insert(
                        call_id.clone(),
                        UserContent::tool_result(
                            call_id,
                            ToolResultContent::from_tool_output(output),
                        ),
                    );
                    save_checkpoint(
                        store,
                        run,
                        machine.clone(),
                        completed_tools.clone(),
                        updates,
                    )?;
                }

                let results = calls
                    .iter()
                    .filter_map(|call| completed_tools.get(&call.tool_call.id).cloned())
                    .collect::<Vec<_>>();
                if results.len() != calls.len() {
                    anyhow::bail!("task tool ledger is incomplete");
                }
                machine.tool_results(results).map_err(anyhow::Error::from)?;
                completed_tools.clear();
                save_checkpoint(
                    store,
                    run,
                    machine.clone(),
                    completed_tools.clone(),
                    updates,
                )?;
            }
            AgentRunStep::Done(response) => {
                run.complete_step(response.output);
                save_and_publish(store, run, updates)?;
                return Ok(());
            }
        }
    }
}

async fn execute_task_tool<M: CompletionModel>(
    agent: &Agent<M>,
    approvals: &ConfirmState,
    tool_name: &str,
    args: &str,
    cancel: &mut watch::Receiver<bool>,
) -> Result<String> {
    match permissions::permission_for_tool(tool_name, args) {
        PermissionResult::Denied(reason) => return Ok(format!("PERMISSION DENIED: {reason}")),
        PermissionResult::Confirm(reason) => {
            let approved = tokio::select! {
                _ = wait_for_cancellation(cancel) => return Ok("TASK CANCELLED before tool approval".into()),
                approved = approvals.request(tool_name, args, Some(&reason)) => approved,
            };
            if !approved {
                return Ok(format!("TOOL DENIED: {reason}"));
            }
            permissions::record_approval(tool_name, args);
        }
        PermissionResult::Allowed => {}
    }

    tokio::select! {
        _ = wait_for_cancellation(cancel) => Ok("TASK CANCELLED before tool execution".into()),
        output = agent.tool_server_handle.call_tool(tool_name, args) => {
            Ok(output.unwrap_or_else(|error| format!("TOOL EXECUTION FAILED: {error}")))
        }
    }
}

fn save_checkpoint(
    store: &TaskStore,
    run: &mut TaskRun,
    agent_run: AgentRun,
    tool_results: BTreeMap<String, UserContent>,
    updates: Option<&mpsc::UnboundedSender<TaskNotification>>,
) -> Result<()> {
    run.checkpoint = Some(TaskCheckpoint {
        agent_run,
        tool_results,
    });
    save_and_publish(store, run, updates)
}

fn save_and_publish(
    store: &TaskStore,
    run: &TaskRun,
    updates: Option<&mpsc::UnboundedSender<TaskNotification>>,
) -> Result<()> {
    store.save(run)?;
    if let Some(updates) = updates {
        let _ = updates.send(TaskNotification::Updated(run.view()));
    }
    Ok(())
}

fn cancel_run(
    store: &TaskStore,
    run: &mut TaskRun,
    updates: Option<&mpsc::UnboundedSender<TaskNotification>>,
) -> Result<()> {
    run.status = TaskRunStatus::Cancelled;
    if let Some(step) = run.steps.get_mut(run.current_step) {
        step.status = TaskStepStatus::Cancelled;
        step.finished_at = Some(unix_millis());
    }
    run.record_event(
        TaskEventKind::Cancelled,
        Some(run.current_step),
        "Task run cancelled",
        "The durable checkpoint was retained and can be resumed.",
    );
    save_and_publish(store, run, updates)
}

fn build_step_prompt(run: &TaskRun, kind: TaskStepKind) -> String {
    let context = run.context_outputs();
    let instructions = match kind {
        TaskStepKind::Plan => {
            "Inspect the repository using only read tools. Do not modify files or execute shell commands. Produce a concrete implementation plan grounded in the actual code, including risks and verification commands."
        }
        TaskStepKind::Implement => {
            "Implement the objective now. Use tools to inspect and edit real files, preserve unrelated user changes, and run focused checks. Do not stop at a proposal. End with a concise account of changed files and checks run."
        }
        TaskStepKind::Verify => {
            "Verify the implementation without editing files. Inspect relevant code and run the strongest focused tests and static checks available. Report exact failures. End with exactly one line: VERDICT: PASS or VERDICT: FAIL."
        }
        TaskStepKind::Review => {
            "Act as a strict senior reviewer. Check the objective, implementation, and verification evidence against the repository. Look for behavioral bugs, unsafe assumptions, regressions, and missing tests. Do not edit files. End with exactly one line: VERDICT: APPROVED or VERDICT: CHANGES_REQUIRED."
        }
        TaskStepKind::Repair => {
            "Repair every actionable defect from the latest review. Inspect and edit real files, keep the changes scoped, and run focused checks. Do not merely explain the repair."
        }
        TaskStepKind::Finalize if run.memory_writes => {
            "Produce the final engineering report in your response: outcome, changed files, verification evidence, and residual risks. Be honest about unresolved review findings. You may store one concise durable project fact in graph memory, but even after a memory tool call your response must still contain the complete engineering report. Do not re-run checks."
        }
        TaskStepKind::Finalize => {
            "Produce the final engineering report in your response: outcome, changed files, verification evidence, and residual risks. Be honest about unresolved review findings. Persistent memory writes are disabled for this run. Do not call graph_store and do not re-run checks."
        }
    };
    format!(
        "You are executing a durable UIntellAgent coding run.\n\
         Workspace: {}\n\
         Objective:\n{}\n\n\
         Current phase: {}\n\
         Memory writes: {}\n\
         Phase requirements:\n{}\n\n\
         Evidence from completed phases:\n{}",
        run.workspace.display(),
        run.objective,
        kind.title(),
        if run.memory_writes {
            "explicitly enabled"
        } else {
            "disabled"
        },
        instructions,
        if context.is_empty() {
            "No completed phases yet."
        } else {
            &context
        }
    )
}

fn build_final_report(run: &TaskRun, finalizer_output: Option<&str>) -> String {
    let implementation = run
        .steps
        .iter()
        .rev()
        .find(|step| matches!(step.kind, TaskStepKind::Repair | TaskStepKind::Implement))
        .and_then(|step| step.output.as_deref())
        .unwrap_or("No implementation evidence was recorded.");
    let verification = run
        .steps
        .iter()
        .rev()
        .find(|step| step.kind == TaskStepKind::Verify)
        .and_then(|step| step.output.as_deref())
        .unwrap_or("No verification evidence was recorded.");
    let review = run
        .steps
        .iter()
        .rev()
        .find(|step| step.kind == TaskStepKind::Review)
        .and_then(|step| step.output.as_deref())
        .unwrap_or("No review evidence was recorded.");
    let finalizer_output = finalizer_output
        .filter(|output| !output.trim().is_empty())
        .unwrap_or("No additional finalization note was produced.");

    format!(
        "# Engineering Report\n\n\
         Outcome: {}\n\
         Objective: {}\n\
         Workspace: {}\n\
         Provider: {}\n\
         Repair rounds: {}\n\n\
         ## Implementation Evidence\n{}\n\n\
         ## Verification Evidence\n{}\n\n\
         ## Review Evidence\n{}\n\n\
         ## Finalization Note\n{}",
        run.status.label(),
        run.objective,
        run.workspace.display(),
        run.provider,
        run.repair_rounds,
        truncate_chars(implementation, 12_000),
        truncate_chars(verification, 12_000),
        truncate_chars(review, 12_000),
        truncate_chars(finalizer_output, 8_000),
    )
}

fn final_verdict(output: &str) -> Option<&str> {
    output
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| line.trim().strip_prefix("VERDICT:"))
        .map(str::trim)
}

fn validate_objective(objective: &str) -> Result<()> {
    let trimmed = objective.trim();
    if trimmed.is_empty() {
        anyhow::bail!("task objective cannot be empty");
    }
    if objective.chars().count() > MAX_OBJECTIVE_CHARS {
        anyhow::bail!("task objective exceeds {MAX_OBJECTIVE_CHARS} characters");
    }
    Ok(())
}

fn validate_run_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 96
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        anyhow::bail!("invalid task run id");
    }
    Ok(())
}

fn validate_run(run: &TaskRun) -> Result<()> {
    if run.version != TASK_RUN_VERSION {
        anyhow::bail!("unsupported task run version {}", run.version);
    }
    validate_run_id(&run.id)?;
    validate_objective(&run.objective)?;
    if !run.workspace.is_absolute() {
        anyhow::bail!("task workspace must be absolute");
    }
    if run.current_step > run.steps.len() {
        anyhow::bail!("task current-step index is invalid");
    }
    if run.events.len() > MAX_EVENTS {
        anyhow::bail!("task event log exceeds {MAX_EVENTS} entries");
    }
    Ok(())
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "task path has no parent")
    })?;
    let temporary = parent.join(format!(
        ".{}-{}-{:x}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("task-run"),
        std::process::id(),
        rand::random::<u64>()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn lock_owner_is_alive(path: &Path) -> bool {
    let Some(pid) = std::fs::read_to_string(path)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
    else {
        return false;
    };
    #[cfg(unix)]
    {
        // Signal 0 performs existence/permission checking without sending a signal.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

fn cancellation_is_set(cancel: &watch::Receiver<bool>) -> bool {
    *cancel.borrow()
}

async fn wait_for_cancellation(cancel: &mut watch::Receiver<bool>) {
    loop {
        if cancellation_is_set(cancel) {
            return;
        }
        if cancel.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn preview(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (TaskStore, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "uintell-task-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        (TaskStore::at(root.clone()), root)
    }

    #[test]
    fn task_store_round_trips_private_checkpoints() {
        let (store, root) = temp_store();
        let workspace = std::env::current_dir().unwrap();
        let run = store
            .create("Implement durable runs", &workspace, "test-provider")
            .unwrap();
        let restored = store.load(&run.id).unwrap();

        assert_eq!(restored.objective, run.objective);
        assert_eq!(restored.steps.len(), 5);
        assert_eq!(store.list().unwrap().len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.join(format!("{}.json", run.id)))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsafe_run_ids_cannot_escape_the_store() {
        let (store, root) = temp_store();
        assert!(store.load("../../permissions").is_err());
        assert!(store.load("run/other").is_err());
        assert!(!root.exists());
    }

    #[test]
    fn review_gate_inserts_bounded_repair_loops() {
        let mut run = TaskRun::new(
            "test".into(),
            std::env::current_dir().unwrap(),
            "test".into(),
        );
        run.current_step = 3;
        run.complete_step("Defect found.\nVERDICT: CHANGES_REQUIRED".into());
        assert_eq!(run.repair_rounds, 1);
        assert_eq!(run.steps[4].kind, TaskStepKind::Repair);
        assert_eq!(run.steps[5].kind, TaskStepKind::Verify);
        assert_eq!(run.steps[6].kind, TaskStepKind::Review);

        run.current_step = 6;
        run.complete_step("Still broken.\nVERDICT: CHANGES_REQUIRED".into());
        assert_eq!(run.repair_rounds, 2);
        let len_after_second = run.steps.len();
        run.current_step = 9;
        run.complete_step("Still broken.\nVERDICT: CHANGES_REQUIRED".into());
        assert_eq!(run.steps.len(), len_after_second);
        assert!(run.quality_gate_failed);
    }

    #[test]
    fn verification_gate_fails_closed_and_repairs_before_review() {
        let mut run = TaskRun::new(
            "test".into(),
            std::env::current_dir().unwrap(),
            "test".into(),
        );
        run.current_step = 2;
        run.complete_step("Tests failed.\nVERDICT: FAIL".into());

        assert_eq!(run.repair_rounds, 1);
        assert_eq!(run.current_step, 3);
        assert_eq!(run.steps[3].kind, TaskStepKind::Repair);
        assert_eq!(run.steps[4].kind, TaskStepKind::Verify);
        assert_eq!(run.steps[5].kind, TaskStepKind::Review);
    }

    #[test]
    fn missing_review_verdict_does_not_silently_pass() {
        let mut run = TaskRun::new(
            "test".into(),
            std::env::current_dir().unwrap(),
            "test".into(),
        );
        run.current_step = 3;
        run.complete_step("No findings were reported.".into());

        assert_eq!(run.repair_rounds, 1);
        assert_eq!(run.steps[4].kind, TaskStepKind::Repair);
    }

    #[test]
    fn phase_tool_policy_prevents_plan_and_review_writes() {
        assert!(!TaskStepKind::Plan.tool_allowed("file_write", false));
        assert!(TaskStepKind::Plan.tool_allowed("file_read", false));
        assert!(TaskStepKind::Implement.tool_allowed("file_write", false));
        assert!(!TaskStepKind::Review.tool_allowed("file_write", false));
        assert!(!TaskStepKind::Finalize.tool_allowed("graph_store", false));
        assert!(TaskStepKind::Finalize.tool_allowed("graph_store", true));
        assert!(!TaskStepKind::Implement.tool_allowed("graph_forget", false));
    }

    #[test]
    fn final_report_preserves_implementation_verification_and_review_evidence() {
        let mut run = TaskRun::new(
            "create answer.txt".into(),
            std::env::current_dir().unwrap(),
            "test".into(),
        );
        run.status = TaskRunStatus::Completed;
        run.steps[1].output = Some("Created answer.txt".into());
        run.steps[2].output = Some("Exact bytes passed\nVERDICT: PASS".into());
        run.steps[3].output = Some("No findings\nVERDICT: APPROVED".into());

        let report = build_final_report(&run, Some("Memory was unavailable."));
        assert!(report.contains("Outcome: completed"));
        assert!(report.contains("Created answer.txt"));
        assert!(report.contains("Exact bytes passed"));
        assert!(report.contains("No findings"));
        assert!(report.contains("Memory was unavailable."));
    }

    #[test]
    fn active_run_lease_blocks_duplicate_drivers_and_recovers_stale_locks() {
        let (store, root) = temp_store();
        let workspace = std::env::current_dir().unwrap();
        let run = store.create("test lease", &workspace, "test").unwrap();
        let lease = store.acquire(&run.id).unwrap();
        assert!(store.acquire(&run.id).is_err());
        drop(lease);
        assert!(store.acquire(&run.id).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn orphaned_running_checkpoint_is_presented_as_paused() {
        let (store, root) = temp_store();
        let workspace = std::env::current_dir().unwrap();
        let mut run = store
            .create("recover interrupted run", &workspace, "test")
            .unwrap();
        run.status = TaskRunStatus::Running;
        run.steps[0].status = TaskStepStatus::Running;
        store.save(&run).unwrap();

        let restored = store.load(&run.id).unwrap();
        assert_eq!(restored.status, TaskRunStatus::Paused);
        assert_eq!(restored.steps[0].status, TaskStepStatus::Pending);
        std::fs::remove_dir_all(root).unwrap();
    }
}
