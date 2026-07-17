// UIntell Agent — Rust-native AI agent with graph memory, provider mesh, gateway
//
// Default provider: DeepSeek (reads DEEPSEEK_API_KEY from env)
mod confirm;
mod db_tui;
mod editor;
mod gateway;
mod http_body;
mod knowledge_graph;
mod lsp;
mod mesh;
mod permissions;
mod provider_health;
mod rig_runtime;
mod session;
mod skills;
mod task_run;
mod tool_result;
mod tools;
mod tui;

use anyhow::Context;
use clap::Parser;
use rig_core::agent::Agent;
use rig_core::client::{CompletionClient, Nothing, ProviderClient};
use rig_core::completion::{CompletionModel, Message, Prompt};
use rig_core::providers::{deepseek, ollama};
use std::io::{self, Write};
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "uintell-agent",
    version,
    about = "Rust-native AI agent with a TUI, tools, and graph memory"
)]
struct Cli {
    #[arg(short, long)]
    prompt: Option<String>,

    #[arg(long)]
    tui: bool,

    /// Use local Ollama instead of DeepSeek
    #[arg(long)]
    ollama: bool,

    /// Show every model turn, tool call, and tool result for --prompt.
    #[arg(long)]
    visible: bool,

    /// Model name (for --ollama)
    #[arg(
        long,
        default_value = "hf.co/unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF:UD-Q4_K_XL"
    )]
    model: String,

    /// Max agent turns for visible stepping.
    #[arg(long, default_value_t = 12)]
    max_turns: usize,

    /// Add a local instruction skill from ~/.uintell/skills.
    #[arg(long = "skill", value_name = "NAME")]
    selected_skills: Vec<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    Serve {
        #[arg(long, default_value = "127.0.0.1:3000")]
        addr: String,
    },
    Init,
    Skills,
    SkillNew {
        name: String,
        description: String,
    },
    /// Launch Yazi-style database manager TUI
    Db,
    /// Print the active, partial, and planned Rig capability map.
    Capabilities,
    /// Verify the provider, graph memory, permissions, and code runtimes.
    Doctor,
    /// Create, inspect, resume, and monitor durable autonomous coding runs.
    Task {
        #[command(subcommand)]
        action: TaskCommand,
    },
    /// Run a prompt through the visible Rig AgentRun state machine.
    Step {
        prompt: String,
        #[arg(long, default_value_t = 12)]
        max_turns: usize,
    },
    /// Route a task into the right UIntellAgent mode.
    Route {
        task: String,
    },
    /// Run a Rig-style prompt chain over a task.
    Chain {
        task: String,
    },
    /// Run planner/coder/reviewer/tester orchestration over a task.
    Orchestrate {
        task: String,
    },
    /// Run evaluator/optimizer workflow over a task.
    Evaluate {
        task: String,
    },
}

#[derive(clap::Subcommand)]
enum TaskCommand {
    /// Start a quality-gated coding run in the current workspace.
    Start {
        /// Allow this run to create or mutate persistent graph memory.
        #[arg(long)]
        remember: bool,
        objective: String,
    },
    /// Resume or retry a paused, cancelled, interrupted, or failed run.
    Resume { id: String },
    /// List recent durable runs.
    List,
    /// Show one run's steps, events, and final result.
    Show { id: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let preamble = skills::compose_preamble(SYSTEM_PROMPT, &cli.selected_skills)?;
    let preamble = preamble.as_str();

    if let Some(cmd) = cli.command {
        if command_uses_provider(&cmd) {
            require_provider_ready(cli.ollama, &cli.model).await?;
        }
        match cmd {
            Command::Serve { addr } => {
                if cli.ollama {
                    let client = ollama::Client::new(Nothing)?;
                    let agent = build_ollama_agent(
                        &client,
                        &cli.model,
                        create_non_interactive_hook(),
                        preamble,
                    );
                    return gateway::serve(agent, &format!("ollama:{}", cli.model), &addr).await;
                } else {
                    let client = deepseek_client()?;
                    let agent =
                        build_deepseek_agent(&client, create_non_interactive_hook(), preamble);
                    return gateway::serve(agent, "deepseek-v4-pro", &addr).await;
                }
            }
            Command::Init => {
                println!("Initializing graph memory...");
                tools::graph::init_schema().await?;
                println!("Done.");
                return Ok(());
            }
            Command::Skills => {
                let list = skills::list_skills()?;
                if list.is_empty() {
                    println!("No skills.");
                } else {
                    for skill in &list {
                        println!(
                            "  {} v{} — {} ({})",
                            skill.name, skill.version, skill.description, skill.entrypoint
                        );
                    }
                }
                return Ok(());
            }
            Command::SkillNew { name, description } => {
                skills::create_skill(&name, &description)?;
                println!("Created skill: {name}");
                println!(
                    "Edit: {}",
                    skills::skills_dir()?.join(&name).join("SKILL.md").display()
                );
                println!("Enable: uintell-agent --skill {name} --tui");
                return Ok(());
            }
            Command::Db => {
                tools::graph::ensure_ready()
                    .await
                    .map_err(anyhow::Error::msg)?;
                return db_tui::run().await;
            }
            Command::Capabilities => {
                rig_runtime::print_capabilities();
                return Ok(());
            }
            Command::Doctor => {
                return run_doctor(cli.ollama, &cli.model).await;
            }
            Command::Task { action } => {
                return run_task_command(action, cli.ollama, &cli.model, preamble).await;
            }
            Command::Step { prompt, max_turns } => {
                ensure_graph_memory_ready().await;
                if cli.ollama {
                    let client = ollama_client()?;
                    let agent = build_ollama_agent(
                        &client,
                        &cli.model,
                        confirm::ConfirmHook::cli_interactive(),
                        preamble,
                    );
                    let output = rig_runtime::run_visible(&agent, &prompt, max_turns).await?;
                    println!("\n{output}");
                } else {
                    let client = deepseek_client()?;
                    let agent = build_deepseek_agent(
                        &client,
                        confirm::ConfirmHook::cli_interactive(),
                        preamble,
                    );
                    let output = rig_runtime::run_visible(&agent, &prompt, max_turns).await?;
                    println!("\n{output}");
                }
                return Ok(());
            }
            Command::Route { task } => {
                ensure_graph_memory_ready().await;
                if cli.ollama {
                    let client = ollama_client()?;
                    let agent = build_ollama_agent(
                        &client,
                        &cli.model,
                        confirm::ConfirmHook::non_interactive(),
                        preamble,
                    );
                    println!("{}", rig_runtime::route(&agent, &task).await?);
                } else {
                    let client = deepseek_client()?;
                    let agent = build_deepseek_agent(
                        &client,
                        confirm::ConfirmHook::non_interactive(),
                        preamble,
                    );
                    println!("{}", rig_runtime::route(&agent, &task).await?);
                }
                return Ok(());
            }
            Command::Chain { task } => {
                ensure_graph_memory_ready().await;
                if cli.ollama {
                    let client = ollama_client()?;
                    let agent = build_ollama_agent(
                        &client,
                        &cli.model,
                        confirm::ConfirmHook::non_interactive(),
                        preamble,
                    );
                    println!("{}", rig_runtime::chain(&agent, &task).await?);
                } else {
                    let client = deepseek_client()?;
                    let agent = build_deepseek_agent(
                        &client,
                        confirm::ConfirmHook::non_interactive(),
                        preamble,
                    );
                    println!("{}", rig_runtime::chain(&agent, &task).await?);
                }
                return Ok(());
            }
            Command::Orchestrate { task } => {
                ensure_graph_memory_ready().await;
                if cli.ollama {
                    let client = ollama_client()?;
                    let agent = build_ollama_agent(
                        &client,
                        &cli.model,
                        confirm::ConfirmHook::non_interactive(),
                        preamble,
                    );
                    println!("{}", rig_runtime::orchestrate(&agent, &task).await?);
                } else {
                    let client = deepseek_client()?;
                    let agent = build_deepseek_agent(
                        &client,
                        confirm::ConfirmHook::non_interactive(),
                        preamble,
                    );
                    println!("{}", rig_runtime::orchestrate(&agent, &task).await?);
                }
                return Ok(());
            }
            Command::Evaluate { task } => {
                ensure_graph_memory_ready().await;
                if cli.ollama {
                    let client = ollama_client()?;
                    let agent = build_ollama_agent(
                        &client,
                        &cli.model,
                        confirm::ConfirmHook::non_interactive(),
                        preamble,
                    );
                    println!("{}", rig_runtime::evaluate_optimize(&agent, &task).await?);
                } else {
                    let client = deepseek_client()?;
                    let agent = build_deepseek_agent(
                        &client,
                        confirm::ConfirmHook::non_interactive(),
                        preamble,
                    );
                    println!("{}", rig_runtime::evaluate_optimize(&agent, &task).await?);
                }
                return Ok(());
            }
        }
    }

    if !cli.tui {
        tracing_subscriber::fmt::init();
    }

    ensure_graph_memory_ready().await;

    if cli.ollama {
        let client = ollama_client()?;
        let provider_health = provider_health::check_ollama(&cli.model).await;
        if !cli.tui {
            provider_health.require_ready()?;
        }
        let confirm_state = confirm::ConfirmState::new();
        let agent = if cli.tui {
            let hook = confirm::ConfirmHook::interactive(confirm_state.clone());
            build_ollama_agent(&client, &cli.model, hook, preamble)
        } else if cli.prompt.is_some() {
            build_ollama_agent(
                &client,
                &cli.model,
                confirm::ConfirmHook::non_interactive(),
                preamble,
            )
        } else {
            build_ollama_agent(
                &client,
                &cli.model,
                confirm::ConfirmHook::cli_interactive(),
                preamble,
            )
        };
        let label = format!("Ollama {}", cli.model);

        if let Some(prompt) = cli.prompt {
            println!("UIntell Agent — {label}\n");
            if cli.visible {
                println!(
                    "{}",
                    rig_runtime::run_visible(&agent, &prompt, cli.max_turns).await?
                );
            } else {
                println!(
                    "{}",
                    agent
                        .prompt(&prompt)
                        .max_turns(rig_runtime::normalize_max_turns(cli.max_turns))
                        .await?
                );
            }
        } else if cli.tui {
            tui::run(agent, &label, confirm_state, provider_health).await?;
        } else {
            println!("UIntell Agent v{} — {label}", env!("CARGO_PKG_VERSION"));
            println!("/exit /graph /skills | --tui for TUI\n");
            interactive_chat(agent).await?;
        }
    } else {
        let client = deepseek_client()?;
        let provider_health = provider_health::check_deepseek().await;
        if !cli.tui {
            provider_health.require_ready()?;
        }
        let confirm_state = confirm::ConfirmState::new();
        let agent = if cli.tui {
            let hook = confirm::ConfirmHook::interactive(confirm_state.clone());
            build_deepseek_agent(&client, hook, preamble)
        } else if cli.prompt.is_some() {
            build_deepseek_agent(&client, confirm::ConfirmHook::non_interactive(), preamble)
        } else {
            build_deepseek_agent(&client, confirm::ConfirmHook::cli_interactive(), preamble)
        };
        let label = "DeepSeek V4 Pro";

        if let Some(prompt) = cli.prompt {
            println!("UIntell Agent — {label}\n");
            if cli.visible {
                println!(
                    "{}",
                    rig_runtime::run_visible(&agent, &prompt, cli.max_turns).await?
                );
            } else {
                println!(
                    "{}",
                    agent
                        .prompt(&prompt)
                        .max_turns(rig_runtime::normalize_max_turns(cli.max_turns))
                        .await?
                );
            }
        } else if cli.tui {
            tui::run(agent, label, confirm_state, provider_health).await?;
        } else {
            println!("UIntell Agent v{} — {label}", env!("CARGO_PKG_VERSION"));
            println!("/exit /graph /skills | --tui for TUI\n");
            interactive_chat(agent).await?;
        }
    }

    Ok(())
}

fn create_non_interactive_hook() -> confirm::ConfirmHook {
    confirm::ConfirmHook::non_interactive()
}

fn command_uses_provider(command: &Command) -> bool {
    matches!(
        command,
        Command::Serve { .. }
            | Command::Step { .. }
            | Command::Route { .. }
            | Command::Chain { .. }
            | Command::Orchestrate { .. }
            | Command::Evaluate { .. }
            | Command::Task {
                action: TaskCommand::Start { .. } | TaskCommand::Resume { .. }
            }
    )
}

async fn require_provider_ready(use_ollama: bool, model: &str) -> anyhow::Result<()> {
    let health = if use_ollama {
        provider_health::check_ollama(model).await
    } else {
        provider_health::check_deepseek().await
    };
    health.require_ready()
}

async fn run_task_command(
    action: TaskCommand,
    use_ollama: bool,
    model: &str,
    preamble: &str,
) -> anyhow::Result<()> {
    let store = task_run::TaskStore::default();
    let mut run = match action {
        TaskCommand::List => {
            let runs = store.list()?;
            if runs.is_empty() {
                println!("No durable task runs.");
            } else {
                println!("{:<30} {:<16} {:<9} OBJECTIVE", "RUN", "STATUS", "STEPS");
                for run in runs {
                    println!(
                        "{:<30} {:<16} {:>3}/{:<5} {}",
                        run.id,
                        run.status.label(),
                        run.current_step.min(run.total_steps),
                        run.total_steps,
                        run.objective
                    );
                }
            }
            return Ok(());
        }
        TaskCommand::Show { id } => {
            print_task_view(&store.load(&id)?.view());
            return Ok(());
        }
        TaskCommand::Start {
            objective,
            remember,
        } => {
            let workspace = std::env::current_dir()?;
            let provider = if use_ollama {
                format!("ollama:{model}")
            } else {
                "deepseek-v4-pro".into()
            };
            if remember {
                store.create_with_memory(objective, &workspace, provider, true)?
            } else {
                store.create(objective, &workspace, provider)?
            }
        }
        TaskCommand::Resume { id } => {
            let mut run = store.load(&id)?;
            if !run.status.is_resumable() {
                anyhow::bail!("task run {id} is already {}", run.status.label());
            }
            std::env::set_current_dir(&run.workspace)
                .with_context(|| format!("enter task workspace {}", run.workspace.display()))?;
            run.provider = if use_ollama {
                format!("ollama:{model}")
            } else {
                "deepseek-v4-pro".into()
            };
            run
        }
    };

    ensure_graph_memory_ready().await;
    println!("Task run: {}", run.id);
    println!("Workspace: {}", run.workspace.display());
    println!("Objective: {}\n", run.objective);

    let approvals = Arc::new(confirm::ConfirmState::cli());
    if use_ollama {
        let client = ollama_client()?;
        let agent = build_ollama_agent(
            &client,
            model,
            confirm::ConfirmHook::non_interactive(),
            preamble,
        );
        run = drive_task_cli(store, run, agent, approvals).await?;
    } else {
        let client = deepseek_client()?;
        let agent =
            build_deepseek_agent(&client, confirm::ConfirmHook::non_interactive(), preamble);
        run = drive_task_cli(store, run, agent, approvals).await?;
    }

    println!();
    print_task_view(&run.view());
    Ok(())
}

async fn drive_task_cli<M: CompletionModel + 'static>(
    store: task_run::TaskStore,
    run: task_run::TaskRun,
    agent: Agent<M>,
    approvals: Arc<confirm::ConfirmState>,
) -> anyhow::Result<task_run::TaskRun> {
    let (updates_tx, mut updates_rx) = tokio::sync::mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        let mut last_sequence = 0;
        while let Some(notification) = updates_rx.recv().await {
            match notification {
                task_run::TaskNotification::Updated(view) => {
                    for event in &view.events {
                        if event.sequence <= last_sequence {
                            continue;
                        }
                        println!(
                            "[{}] {}{}",
                            event.kind.label(),
                            event.title,
                            if event.detail.is_empty() {
                                String::new()
                            } else {
                                format!(" · {}", event.detail.replace('\n', " "))
                            }
                        );
                        last_sequence = event.sequence;
                    }
                }
                task_run::TaskNotification::DriverError { id, error } => {
                    eprintln!("[failed] {id}: {error}");
                }
            }
        }
    });
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let signal_tx = cancel_tx.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = signal_tx.send(true);
        }
    });

    let result = task_run::execute(
        store,
        run,
        Arc::new(agent),
        approvals,
        Some(updates_tx),
        cancel_rx,
    )
    .await;
    signal.abort();
    drop(cancel_tx);
    let _ = printer.await;
    result
}

fn print_task_view(view: &task_run::TaskView) {
    println!("{} · {}", view.id, view.status.label());
    println!("Provider: {}", view.provider);
    println!(
        "Memory writes: {}",
        if view.memory_writes {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("Workspace: {}", view.workspace.display());
    println!("Objective: {}", view.objective);
    println!("Repair rounds: {}", view.repair_rounds);
    println!("Steps:");
    for (index, step) in view.steps.iter().enumerate() {
        println!(
            "  {:>2}. [{:<9}] {} (attempt {})",
            index + 1,
            step.status.label(),
            step.title,
            step.attempt
        );
        if let Some(error) = &step.error {
            println!("      error: {error}");
        }
    }
    if let Some(error) = &view.error {
        println!("Error: {error}");
    }
    if let Some(result) = &view.result {
        println!("\nResult:\n{result}");
    }
}

#[derive(Default)]
struct DoctorReport {
    failures: usize,
    warnings: usize,
}

impl DoctorReport {
    fn ok(&self, name: &str, detail: impl std::fmt::Display) {
        println!("[ok]   {name:<14} {detail}");
    }

    fn fail(&mut self, name: &str, detail: impl std::fmt::Display) {
        self.failures += 1;
        println!("[fail] {name:<14} {detail}");
    }

    fn warn(&mut self, name: &str, detail: impl std::fmt::Display) {
        self.warnings += 1;
        println!("[warn] {name:<14} {detail}");
    }
}

async fn run_doctor(use_ollama: bool, model: &str) -> anyhow::Result<()> {
    println!("UIntell Agent doctor\n");
    let mut report = DoctorReport::default();

    let provider = if use_ollama {
        provider_health::check_ollama(model).await
    } else {
        provider_health::check_deepseek().await
    };
    if provider.is_ready() {
        report.ok("provider", provider.detail());
    } else {
        report.fail("provider", provider.detail());
    }

    match tools::graph::ensure_ready().await {
        Ok(()) => report.ok("graph memory", "SurrealDB and schema are ready"),
        Err(error) => report.fail("graph memory", error),
    }

    let permissions_config = permissions::PermissionsConfig::load();
    match permissions::config_path() {
        Ok(permissions_path) => {
            match std::fs::read_to_string(&permissions_path)
                .map_err(anyhow::Error::from)
                .and_then(|value| {
                    let config = toml::from_str::<permissions::PermissionsConfig>(&value)?;
                    config.validate().map_err(anyhow::Error::msg)?;
                    Ok(config)
                }) {
                Ok(_) => report.ok(
                    "permissions",
                    format!(
                        "{:?} · {}",
                        permissions_config.mode,
                        permissions_path.display()
                    ),
                ),
                Err(error) => report.fail(
                    "permissions",
                    format!("{} is invalid: {error}", permissions_path.display()),
                ),
            }
        }
        Err(error) => report.fail("permissions", error),
    }

    match permissions::permission_for_tool(
        "code_exec",
        r#"{"language":"python","code":"print('doctor')"}"#,
    ) {
        permissions::PermissionResult::Allowed => report.ok("code policy", "code_exec is allowed"),
        permissions::PermissionResult::Confirm(reason) => report.warn("code policy", reason),
        permissions::PermissionResult::Denied(reason) => report.fail("code policy", reason),
    }

    let cwd = std::env::current_dir()?;
    let write_probe = serde_json::json!({"path": cwd.join(".uintell-doctor-probe")}).to_string();
    match permissions::permission_for_tool("file_write", &write_probe) {
        permissions::PermissionResult::Allowed => report.ok(
            "file policy",
            format!("writes allowed in {}", cwd.display()),
        ),
        permissions::PermissionResult::Confirm(reason) => report.warn("file policy", reason),
        permissions::PermissionResult::Denied(reason) => report.fail("file policy", reason),
    }

    match tools::code::self_test().await {
        Ok(()) => report.ok("code sandbox", "Python and Rust execute inside bubblewrap"),
        Err(error) => report.fail("code sandbox", error),
    }

    for runtime in ["bash", "node"] {
        match executable_version(runtime) {
            Some(version) => report.ok(runtime, version),
            None => report.fail(runtime, "not found or failed to start"),
        }
    }
    match executable_version("rust-analyzer") {
        Some(version) => report.ok("code intel", version),
        None => report.warn(
            "code intel",
            "rust-analyzer is unavailable; editor completion falls back to buffer words",
        ),
    }
    if executable_version("surreal").is_none() {
        report.warn(
            "surreal binary",
            "not found; graph memory cannot auto-start after a reboot",
        );
    } else {
        report.ok("surreal binary", "installed");
    }

    match workspace_write_probe() {
        Ok(path) => report.ok("state storage", format!("writable at {}", path.display())),
        Err(error) => report.fail("state storage", error),
    }

    println!(
        "\nDoctor finished: {} failure(s), {} warning(s)",
        report.failures, report.warnings
    );
    if report.failures > 0 {
        anyhow::bail!("UIntell Agent is not fully operational")
    }
    Ok(())
}

fn executable_version(program: &str) -> Option<String> {
    let output = std::process::Command::new(program)
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("available")
        .trim()
        .to_string();
    Some(if line.is_empty() {
        "available".into()
    } else {
        line
    })
}

fn workspace_write_probe() -> std::io::Result<std::path::PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let directory = std::path::PathBuf::from(home).join(".uintell");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!(".doctor-{}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(b"uintell-doctor")?;
        file.sync_all()
    })();
    let _ = std::fs::remove_file(&path);
    result.map(|()| directory)
}

async fn ensure_graph_memory_ready() {
    if let Err(e) = tools::graph::ensure_ready().await {
        eprintln!("Graph memory warning: {e}");
    }
}

fn ollama_client() -> anyhow::Result<ollama::Client> {
    Ok(ollama::Client::new(Nothing)?)
}

fn deepseek_client() -> anyhow::Result<deepseek::Client> {
    if std::env::var("DEEPSEEK_API_KEY")
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        anyhow::bail!(
            "No DEEPSEEK_API_KEY found.\n\
             Set it: export DEEPSEEK_API_KEY=\"sk-...\"\n\
             Or use: uintell-agent --ollama"
        );
    }

    Ok(deepseek::Client::from_env()?)
}

fn build_ollama_agent(
    client: &ollama::Client,
    model: &str,
    hook: confirm::ConfirmHook,
    preamble: &str,
) -> Agent<ollama::CompletionModel> {
    client
        .agent(model)
        .preamble(preamble)
        .add_hook(hook)
        .tool(tools::Terminal)
        .tool(tools::FileRead)
        .tool(tools::FileWrite)
        .tool(tools::Browser)
        .tool(tools::WebSearch)
        .tool(tools::CodeExec)
        .tool(tools::FileSearch)
        .tool(tools::GraphStore)
        .tool(tools::GraphQuery)
        .tool(tools::GraphContext)
        .tool(tools::GraphEdit)
        .tool(tools::GraphForget)
        .tool(tools::ProviderMesh)
        .build()
}

fn build_deepseek_agent(
    client: &deepseek::Client,
    hook: confirm::ConfirmHook,
    preamble: &str,
) -> Agent<deepseek::CompletionModel> {
    client
        .agent(deepseek::DEEPSEEK_V4_PRO)
        .preamble(preamble)
        .add_hook(hook)
        .tool(tools::Terminal)
        .tool(tools::FileRead)
        .tool(tools::FileWrite)
        .tool(tools::Browser)
        .tool(tools::WebSearch)
        .tool(tools::CodeExec)
        .tool(tools::FileSearch)
        .tool(tools::GraphStore)
        .tool(tools::GraphQuery)
        .tool(tools::GraphContext)
        .tool(tools::GraphEdit)
        .tool(tools::GraphForget)
        .tool(tools::ProviderMesh)
        .build()
}

async fn interactive_chat<M: CompletionModel + 'static>(agent: Agent<M>) -> anyhow::Result<()> {
    let mut history = Vec::<Message>::new();
    loop {
        print!("> ");
        io::stdout().flush()?;
        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            break;
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if input == "/exit" || input == "/quit" {
            println!("bye.");
            break;
        }
        if input == "/graph" {
            match tools::graph::init_schema().await {
                Ok(()) => println!("Graph schema initialized."),
                Err(e) => eprintln!("Graph error: {e}"),
            }
            continue;
        }
        if input == "/skills" {
            match skills::list_skills() {
                Ok(list) => {
                    if list.is_empty() {
                        println!("No skills.");
                    } else {
                        for s in &list {
                            println!("  {} v{} — {}", s.name, s.version, s.description);
                        }
                    }
                }
                Err(e) => eprintln!("{e}"),
            }
            continue;
        }
        if let Some(prompt) = input.strip_prefix("/step ") {
            match rig_runtime::run_visible(&agent, prompt.trim(), 12).await {
                Ok(r) => println!("{r}\n"),
                Err(e) => eprintln!("[!] {e}"),
            }
            continue;
        }
        if input == "/capabilities" {
            rig_runtime::print_capabilities();
            continue;
        }
        if let Some(task) = input.strip_prefix("/route ") {
            match rig_runtime::route(&agent, task.trim()).await {
                Ok(r) => println!("{r}\n"),
                Err(e) => eprintln!("[!] {e}"),
            }
            continue;
        }
        if let Some(task) = input.strip_prefix("/chain ") {
            match rig_runtime::chain(&agent, task.trim()).await {
                Ok(r) => println!("{r}\n"),
                Err(e) => eprintln!("[!] {e}"),
            }
            continue;
        }
        if let Some(task) = input.strip_prefix("/orchestrate ") {
            match rig_runtime::orchestrate(&agent, task.trim()).await {
                Ok(r) => println!("{r}\n"),
                Err(e) => eprintln!("[!] {e}"),
            }
            continue;
        }
        if let Some(task) = input.strip_prefix("/evaluate ") {
            match rig_runtime::evaluate_optimize(&agent, task.trim()).await {
                Ok(r) => println!("{r}\n"),
                Err(e) => eprintln!("[!] {e}"),
            }
            continue;
        }
        match agent
            .prompt(input)
            .history(history.clone())
            .max_turns(12)
            .await
        {
            Ok(r) => {
                history.push(Message::user(input));
                history.push(Message::assistant(r.clone()));
                println!("{r}\n");
            }
            Err(e) => eprintln!("[!] {e}"),
        }
    }
    Ok(())
}

const SYSTEM_PROMPT: &str = r#"You are UIntell Agent — a Rust-native AI agent with graph memory, provider mesh, and extensible skills.

TOOLS:
- terminal: run shell commands
- file_read/write: file operations
- file_search: ripgrep code search
- browser: fetch web pages (text or HTML)
- web_search: DuckDuckGo search
- code_exec: run Python, bash, Rust, Node.js
- graph_store: remember facts in persistent graph memory (SurrealDB)
- graph_query: search stored memories
- graph_context: load relevant memories at conversation start
- provider_mesh: query multiple LLMs simultaneously, fastest wins

RULES:
- Answer directly and concisely.
- When the user asks for transparent execution, use visible step mode from the runtime.
- Respect tool permissions, confirmations, and runtime limits.
- Ask for confirmation before destructive, external, or privileged actions unless already approved.
- Prefer verified tool output over guesses.
- Use graph_context at conversation start to recall user/project facts.
- Use graph_store to persist important facts for future sessions.
- Use provider_mesh for fast/diverse responses when needed.
- Execute with tools — produce real output, not descriptions.
"#;
