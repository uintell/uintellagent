use std::collections::BTreeSet;

use anyhow::Result;
use rig_core::agent::run::{AgentRun, AgentRunStep, ModelTurn, ModelTurnOutcome};
use rig_core::agent::InvalidToolCallHookAction;
use rig_core::client::{CompletionClient, ProviderClient};
use rig_core::completion::{Completion, ToolDefinition};
use rig_core::message::{ToolResultContent, UserContent};
use rig_core::providers::deepseek;
use rig_core::tool::Tool;
use serde::Deserialize;
use serde_json::json;

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

#[tokio::main]
async fn main() -> Result<()> {
    let agent = deepseek::Client::from_env()?
        .agent(deepseek::DEEPSEEK_V4_PRO)
        .preamble("You are UIntellAgent. Use tools and expose each step.")
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
