use anyhow::Result;
use rig_core::agent::{AgentHook, Flow, HookContext, StepEvent};
use rig_core::client::{CompletionClient, ProviderClient};
use rig_core::completion::{CompletionModel, Prompt, ToolDefinition};
use rig_core::providers::deepseek;
use rig_core::tool::Tool;
use serde::Deserialize;
use serde_json::json;

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

struct PrintApprovalHook;

impl<M: CompletionModel> AgentHook<M> for PrintApprovalHook {
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
    let agent = deepseek::Client::from_env()?
        .agent(deepseek::DEEPSEEK_V4_PRO)
        .preamble("Use write_note to create a short approved note in /tmp.")
        .tool(WriteNote)
        .add_hook(PrintApprovalHook)
        .build();

    let response = agent
        .prompt("Write 'UIntell approval example' to /tmp/uintell-approval.txt.")
        .await?;

    println!("{response}");
    Ok(())
}
