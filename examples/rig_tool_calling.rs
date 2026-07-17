use anyhow::Result;
use rig_core::client::{CompletionClient, ProviderClient};
use rig_core::completion::{Prompt, ToolDefinition};
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
        .preamble("You are UIntellAgent's calculator example. Use tools for arithmetic.")
        .tool(Add)
        .max_tokens(1024)
        .build();

    let response = agent.prompt("What is 19 + 23?").await?;
    println!("{response}");
    Ok(())
}
