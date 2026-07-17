use std::collections::BTreeSet;

use anyhow::Result;
use rig_core::agent::run::{AgentRun, AgentRunStep, ModelTurn, ModelTurnOutcome};
use rig_core::agent::{Agent, InvalidToolCallHookAction};
use rig_core::completion::{Completion, CompletionModel, Prompt};
use rig_core::message::{ToolResultContent, UserContent};

pub struct Capability {
    pub name: &'static str,
    pub status: CapabilityStatus,
    pub uintell_surface: &'static str,
    pub rig_source: &'static str,
}

#[derive(Clone, Copy)]
pub enum CapabilityStatus {
    Active,
    Partial,
    Planned,
}

impl CapabilityStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Partial => "partial",
            Self::Planned => "planned",
        }
    }
}

pub const CAPABILITIES: &[Capability] = &[
    Capability {
        name: "agents",
        status: CapabilityStatus::Active,
        uintell_surface: "main DeepSeek/Ollama agent builders",
        rig_source: "agent",
    },
    Capability {
        name: "tools",
        status: CapabilityStatus::Active,
        uintell_surface: "src/tools/*",
        rig_source: "agent_with_tools",
    },
    Capability {
        name: "manual visible stepping",
        status: CapabilityStatus::Active,
        uintell_surface: "uintell-agent step <prompt> and /step",
        rig_source: "agent_run_stepping",
    },
    Capability {
        name: "approval hooks",
        status: CapabilityStatus::Active,
        uintell_surface: "src/confirm.rs + src/permissions.rs",
        rig_source: "agent_with_human_in_the_loop",
    },
    Capability {
        name: "approval policies",
        status: CapabilityStatus::Active,
        uintell_surface: "~/.uintell/permissions.toml",
        rig_source: "agent_with_approval_policy",
    },
    Capability {
        name: "streaming chat",
        status: CapabilityStatus::Active,
        uintell_surface: "src/tui.rs",
        rig_source: "agent_stream_chat",
    },
    Capability {
        name: "graph memory",
        status: CapabilityStatus::Active,
        uintell_surface: "src/tools/graph.rs and src/db_tui.rs",
        rig_source: "agent_with_memory + rig-surrealdb",
    },
    Capability {
        name: "RAG/code search",
        status: CapabilityStatus::Partial,
        uintell_surface: "file_search, graph_context, browser/search tools",
        rig_source: "rag + rag_dynamic_tools",
    },
    Capability {
        name: "dynamic tools",
        status: CapabilityStatus::Planned,
        uintell_surface: "embedding-selected UIntell tool registry",
        rig_source: "rag_dynamic_tools_multi_turn",
    },
    Capability {
        name: "multi-turn sessions",
        status: CapabilityStatus::Active,
        uintell_surface: "CLI loop and streaming TUI",
        rig_source: "multi_turn_agent",
    },
    Capability {
        name: "routing",
        status: CapabilityStatus::Active,
        uintell_surface: "uintell-agent route <task>",
        rig_source: "agent_routing",
    },
    Capability {
        name: "prompt chaining",
        status: CapabilityStatus::Active,
        uintell_surface: "uintell-agent chain <task>",
        rig_source: "agent_prompt_chaining",
    },
    Capability {
        name: "multi-agent orchestration",
        status: CapabilityStatus::Active,
        uintell_surface: "uintell-agent orchestrate <task>",
        rig_source: "multi_agent + agent_orchestrator",
    },
    Capability {
        name: "agent as tool",
        status: CapabilityStatus::Planned,
        uintell_surface: "specialist agents exposed as tools",
        rig_source: "agent_with_agent_tool",
    },
    Capability {
        name: "MCP",
        status: CapabilityStatus::Planned,
        uintell_surface: "external tool bridge",
        rig_source: "rmcp",
    },
    Capability {
        name: "observability",
        status: CapabilityStatus::Partial,
        uintell_surface: "tracing + gateway logs",
        rig_source: "agent_with_tools_otel",
    },
    Capability {
        name: "evaluator optimizer",
        status: CapabilityStatus::Active,
        uintell_surface: "uintell-agent evaluate <task>",
        rig_source: "agent_evaluator_optimizer",
    },
];

pub fn print_capabilities() {
    println!("UIntellAgent Rig capability map\n");
    for cap in CAPABILITIES {
        println!(
            "{:<27} {:<8} {:<48} from {}",
            cap.name,
            cap.status.label(),
            cap.uintell_surface,
            cap.rig_source
        );
    }
}

pub async fn run_visible<M>(agent: &Agent<M>, prompt: &str, max_turns: usize) -> Result<String>
where
    M: CompletionModel,
{
    let mut run = AgentRun::new(prompt).max_turns(normalize_max_turns(max_turns));

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
                println!(
                    "done: {} model call(s), {} tokens",
                    response.completion_calls.len(),
                    response.usage.total_tokens
                );
                return Ok(response.output);
            }
        }
    }
}

pub fn normalize_max_turns(max_turns: usize) -> usize {
    if max_turns == 0 {
        12
    } else {
        max_turns.min(128)
    }
}

pub async fn route<M>(agent: &Agent<M>, task: &str) -> Result<String>
where
    M: CompletionModel + 'static,
{
    let categories = [
        "code", "shell", "memory", "research", "database", "workflow", "chat",
    ];
    let prompt = format!(
        "Route this UIntellAgent task into exactly one category.\n\
         Categories: {}\n\
         Return only the category and one short reason.\n\n\
         Task:\n{task}",
        categories.join(", ")
    );
    agent
        .prompt(&prompt)
        .max_turns(12)
        .await
        .map_err(Into::into)
}

pub async fn chain<M>(agent: &Agent<M>, task: &str) -> Result<String>
where
    M: CompletionModel + 'static,
{
    let steps = [
        "Clarify the concrete objective and constraints.",
        "Design the smallest implementation path that can work.",
        "Identify tests or checks that prove the result.",
        "Produce the final execution plan.",
    ];

    let mut state = task.to_string();
    for (idx, step) in steps.iter().enumerate() {
        println!("chain step {}: {step}", idx + 1);
        let prompt = format!(
            "You are UIntellAgent running a Rig-style prompt chain.\n\
             Current step: {step}\n\
             Use the prior state below and return the improved state.\n\n\
             Prior state:\n{state}"
        );
        state = agent.prompt(&prompt).max_turns(12).await?;
    }

    Ok(state)
}

pub async fn orchestrate<M>(agent: &Agent<M>, task: &str) -> Result<String>
where
    M: CompletionModel + 'static,
{
    let planner = agent
        .prompt(&format!(
            "Act as the planner agent. Break this into ordered work:\n{task}"
        ))
        .max_turns(12)
        .await?;
    let coder = agent
        .prompt(&format!(
            "Act as the coder agent. Convert this plan into implementation actions:\n{planner}"
        ))
        .max_turns(12)
        .await?;
    let reviewer = agent
        .prompt(&format!(
            "Act as the reviewer agent. Find risks, missing tests, and bad assumptions:\n{coder}"
        ))
        .max_turns(12)
        .await?;
    let tester = agent
        .prompt(&format!(
            "Act as the tester agent. Create the verification checklist:\n{reviewer}"
        ))
        .max_turns(12)
        .await?;

    Ok(format!(
        "PLANNER\n{planner}\n\nCODER\n{coder}\n\nREVIEWER\n{reviewer}\n\nTESTER\n{tester}"
    ))
}

pub async fn evaluate_optimize<M>(agent: &Agent<M>, task: &str) -> Result<String>
where
    M: CompletionModel + 'static,
{
    let draft = agent
        .prompt(&format!(
            "Generate a first solution for this UIntellAgent task:\n{task}"
        ))
        .max_turns(12)
        .await?;
    let critique = agent
        .prompt(&format!(
            "Evaluate this solution. Be strict. Name defects and missing tests:\n{draft}"
        ))
        .max_turns(12)
        .await?;
    let improved = agent
        .prompt(&format!(
            "Improve the solution using this critique.\n\nSolution:\n{draft}\n\nCritique:\n{critique}"
        ))
        .max_turns(12)
        .await?;

    Ok(format!(
        "DRAFT\n{draft}\n\nCRITIQUE\n{critique}\n\nIMPROVED\n{improved}"
    ))
}

#[cfg(test)]
mod tests {
    use super::normalize_max_turns;

    #[test]
    fn max_turns_never_reaches_rig_as_zero_or_unbounded() {
        assert_eq!(normalize_max_turns(0), 12);
        assert_eq!(normalize_max_turns(12), 12);
        assert_eq!(normalize_max_turns(usize::MAX), 128);
    }
}
