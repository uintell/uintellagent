use std::collections::BTreeSet;

use anyhow::Result;
use rig_core::agent::run::{AgentRun, AgentRunStep, ModelTurn, ModelTurnOutcome};
use rig_core::agent::{AgentHook, Flow, HookContext, InvalidToolCallHookAction, StepEvent};
use rig_core::client::{CompletionClient, ProviderClient};
use rig_core::completion::{Completion, CompletionModel, Prompt, ToolDefinition};
use rig_core::memory::InMemoryConversationMemory;
use rig_core::message::{ToolResultContent, UserContent};
use rig_core::providers::deepseek;
use rig_core::tool::Tool;
use serde::Deserialize;
use serde_json::json;

const PATTERNS: &[(&str, &str, &str)] = &[
    ("agent", "demo", "smallest prompt/response agent"),
    (
        "agent_with_tools",
        "demo",
        "typed tool definitions and tool registry",
    ),
    ("manual_tool_calls", "demo", "explicit tool-call execution"),
    (
        "agent_run_stepping",
        "demo",
        "visible step-by-step agent runtime",
    ),
    (
        "agent_with_human_in_the_loop",
        "demo",
        "tool-call policy hook",
    ),
    (
        "agent_with_approval_policy",
        "demo",
        "policy-driven tool gating",
    ),
    (
        "agent_with_durable_approval",
        "product",
        "resumable approval state",
    ),
    ("agent_with_memory", "demo", "conversation memory"),
    (
        "agent_with_memory_streaming",
        "product",
        "streamed output with memory",
    ),
    ("rag", "partial", "document/codebase retrieval"),
    ("rag_dynamic_tools", "planned", "embedding-selected tools"),
    (
        "rag_dynamic_tools_multi_turn",
        "planned",
        "dynamic tools across a session",
    ),
    (
        "agent_orchestrator",
        "product",
        "planner/orchestrator workflow",
    ),
    (
        "agent_parallelization",
        "planned",
        "parallel specialist agents",
    ),
    ("agent_routing", "demo", "route prompts to specialist flows"),
    ("agent_prompt_chaining", "demo", "multi-stage task chains"),
    ("multi_agent", "demo", "planner/reviewer agent roles"),
    (
        "agent_with_agent_tool",
        "planned",
        "agent-as-tool composition",
    ),
    (
        "reasoning_loop",
        "product",
        "explicit planner/executor loop",
    ),
    ("rmcp", "planned", "MCP tool integration"),
    (
        "rig-surrealdb/vector_search_surreal",
        "partial",
        "SurrealDB vector memory",
    ),
];

#[derive(Deserialize)]
struct OperationArgs {
    x: i32,
    y: i32,
}

#[derive(Debug, thiserror::Error)]
#[error("calculator tool error")]
struct CalculatorError;

struct Add;

impl Tool for Add {
    const NAME: &'static str = "add";
    type Error = CalculatorError;
    type Args = OperationArgs;
    type Output = i32;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Add x and y together.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "x": { "type": "integer" },
                    "y": { "type": "integer" }
                },
                "required": ["x", "y"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        Ok(args.x + args.y)
    }
}

#[derive(Deserialize)]
struct WriteNoteArgs {
    path: String,
    contents: String,
}

#[derive(Debug, thiserror::Error)]
#[error("write note failed: {0}")]
struct WriteNoteError(String);

struct WriteNote;

impl Tool for WriteNote {
    const NAME: &'static str = "write_note";
    type Error = WriteNoteError;
    type Args = WriteNoteArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Write a note to /tmp for the approval example.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "contents": { "type": "string" }
                },
                "required": ["path", "contents"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        if !args.path.starts_with("/tmp/") {
            return Err(WriteNoteError("only /tmp paths are allowed".to_string()));
        }
        tokio::fs::write(&args.path, args.contents)
            .await
            .map_err(|e| WriteNoteError(e.to_string()))?;
        Ok(format!("wrote {}", args.path))
    }
}

struct ApprovalLogger;

impl<M: CompletionModel> AgentHook<M> for ApprovalLogger {
    async fn on_event(&self, _ctx: &HookContext, event: StepEvent<'_, M>) -> Flow {
        if let StepEvent::ToolCall {
            tool_name, args, ..
        } = event
        {
            println!("approval checkpoint: {tool_name}({args})");
            if tool_name == "write_note" && !args.to_string().contains("/tmp/") {
                return Flow::skip("denied: this example only allows /tmp writes");
            }
        }
        Flow::cont()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    print_catalog();

    if std::env::var("DEEPSEEK_API_KEY")
        .unwrap_or_default()
        .is_empty()
    {
        println!("\nDEEPSEEK_API_KEY is not set, so live model sections were skipped.");
        println!("Set DEEPSEEK_API_KEY and rerun: cargo run --example rig_all_in_one");
        return Ok(());
    }

    let client = deepseek::Client::from_env()?;

    simple_agent(&client).await?;
    tool_calling(&client).await?;
    visible_steps(&client).await?;
    approval_policy(&client).await?;
    memory(&client).await?;
    routing(&client).await?;
    prompt_chaining(&client).await?;
    multi_agent_plan(&client).await?;
    graph_and_rag_notes();

    Ok(())
}

fn print_catalog() {
    println!("== Rig examples and UIntellAgent capability status ==");
    for (name, status, purpose) in PATTERNS {
        println!("{name:36} {status:8} {purpose}");
    }
}

async fn simple_agent(client: &deepseek::Client) -> Result<()> {
    section("basic agent");
    let agent = client
        .agent(deepseek::DEEPSEEK_V4_PRO)
        .preamble("You are UIntellAgent. Answer briefly.")
        .build();
    let response = agent
        .prompt("Say UIntellAgent is online in one sentence.")
        .await?;
    println!("{response}");
    Ok(())
}

async fn tool_calling(client: &deepseek::Client) -> Result<()> {
    section("tool calling");
    let agent = client
        .agent(deepseek::DEEPSEEK_V4_PRO)
        .preamble("Use tools for arithmetic. Do not do arithmetic in your head.")
        .tool(Add)
        .build();
    let response = agent.prompt("What is 19 + 23?").await?;
    println!("{response}");
    Ok(())
}

async fn visible_steps(client: &deepseek::Client) -> Result<()> {
    section("visible agent run stepping");
    let agent = client
        .agent(deepseek::DEEPSEEK_V4_PRO)
        .preamble("Use tools and expose each step.")
        .tool(Add)
        .build();

    let mut run = AgentRun::new("Calculate 40 + 2.").max_turns(3);
    loop {
        match run.next_step()? {
            AgentRunStep::CallModel {
                prompt,
                history,
                turn,
            } => {
                println!("model call #{turn}");
                let response = agent.completion(prompt, history).await?.send().await?;
                let tool_names: BTreeSet<String> = agent
                    .tool_server_handle
                    .get_tool_defs(None)
                    .await?
                    .into_iter()
                    .map(|def| def.name)
                    .collect();
                let mut outcome = run.model_response(ModelTurn::new(
                    response.message_id.clone(),
                    response.choice.clone(),
                    response.usage,
                    tool_names.clone(),
                    tool_names,
                ))?;
                while let ModelTurnOutcome::NeedsResolution(context) = outcome {
                    eprintln!("unknown tool requested: {}", context.tool_name);
                    outcome = run.resolve_invalid_tool_call(InvalidToolCallHookAction::fail())?;
                }
            }
            AgentRunStep::CallTools { calls } => {
                let mut results = Vec::new();
                for call in calls {
                    if let Some(result) = call.preresolved_result {
                        results.push(result);
                        continue;
                    }
                    let name = &call.tool_call.function.name;
                    let args = call.tool_call.function.arguments.to_string();
                    println!("tool call: {name}({args})");
                    let output = agent.tool_server_handle.call_tool(name, &args).await?;
                    println!("tool result: {output}");
                    results.push(UserContent::tool_result(
                        call.tool_call.id.clone(),
                        ToolResultContent::from_tool_output(output),
                    ));
                }
                run.tool_results(results)?;
            }
            AgentRunStep::Done(response) => {
                println!("done: {}", response.output);
                println!(
                    "{} model call(s), {} tokens",
                    response.completion_calls.len(),
                    response.usage.total_tokens
                );
                break;
            }
        }
    }
    Ok(())
}

async fn approval_policy(client: &deepseek::Client) -> Result<()> {
    section("approval policy hook");
    let agent = client
        .agent(deepseek::DEEPSEEK_V4_PRO)
        .preamble("Use write_note to create a short approved note in /tmp.")
        .tool(WriteNote)
        .add_hook(ApprovalLogger)
        .build();
    let response = agent
        .prompt("Write 'UIntell all-in-one approval example' to /tmp/uintell-all-in-one.txt.")
        .await?;
    println!("{response}");
    Ok(())
}

async fn memory(client: &deepseek::Client) -> Result<()> {
    section("conversation memory");
    let memory = InMemoryConversationMemory::new();
    let agent = client
        .agent(deepseek::DEEPSEEK_V4_PRO)
        .preamble("You are UIntellAgent with conversation memory.")
        .memory(memory)
        .build();
    let first = agent
        .prompt("Remember this project name: UIntellAgent.")
        .conversation("all-in-one-session")
        .await?;
    println!("turn 1: {first}");
    let second = agent
        .prompt("What project name did I ask you to remember?")
        .conversation("all-in-one-session")
        .await?;
    println!("turn 2: {second}");
    Ok(())
}

async fn routing(client: &deepseek::Client) -> Result<()> {
    section("routing");
    let router = client
        .agent(deepseek::DEEPSEEK_V4_PRO)
        .preamble("Classify the user task as exactly one of: code, memory, shell. Return one word.")
        .build();
    let category = router
        .prompt("Store a fact about the current repository and connect it to a file.")
        .await?;
    println!("router chose: {}", category.trim());
    Ok(())
}

async fn prompt_chaining(client: &deepseek::Client) -> Result<()> {
    section("prompt chaining");
    let planner = client
        .agent(deepseek::DEEPSEEK_V4_PRO)
        .preamble("Create a three-step implementation plan. Be concise.")
        .build();
    let reviewer = client
        .agent(deepseek::DEEPSEEK_V4_PRO)
        .preamble("Review the plan and name the riskiest missing piece. Be concise.")
        .build();
    let plan = planner
        .prompt("Plan a Yazi-style graph database UI for UIntellAgent.")
        .await?;
    println!("plan:\n{plan}");
    let review = reviewer.prompt(&plan).await?;
    println!("review:\n{review}");
    Ok(())
}

async fn multi_agent_plan(client: &deepseek::Client) -> Result<()> {
    section("multi-agent planner/reviewer");
    let architect = client
        .agent(deepseek::DEEPSEEK_V4_PRO)
        .preamble("You are the architect. Propose the module boundary in one paragraph.")
        .build();
    let tester = client
        .agent(deepseek::DEEPSEEK_V4_PRO)
        .preamble("You are the tester. Propose the most important test in one paragraph.")
        .build();
    let task = "Add visible code editing to UIntellAgent.";
    let architecture = architect.prompt(task).await?;
    let tests = tester.prompt(task).await?;
    println!("architect:\n{architecture}");
    println!("tester:\n{tests}");
    Ok(())
}

fn graph_and_rag_notes() {
    section("graph, retrieval, and MCP status");
    println!("SurrealDB graph memory is active in the UIntellAgent product runtime.");
    println!("Code/file retrieval is active; embedding-selected dynamic tools are planned.");
    println!("MCP and agent-as-tool composition are planned and are not claimed as active.");
}

fn section(name: &str) {
    println!("\n== {name} ==");
}
