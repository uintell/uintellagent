use anyhow::Result;
use rig_core::client::{CompletionClient, ProviderClient};
use rig_core::completion::Prompt;
use rig_core::memory::InMemoryConversationMemory;
use rig_core::providers::deepseek;

#[tokio::main]
async fn main() -> Result<()> {
    let memory = InMemoryConversationMemory::new();
    let agent = deepseek::Client::from_env()?
        .agent(deepseek::DEEPSEEK_V4_PRO)
        .preamble("You are UIntellAgent with conversation memory.")
        .memory(memory)
        .build();

    let first = agent
        .prompt("Remember this project name: UIntellAgent.")
        .conversation("local-dev-session")
        .await?;
    println!("turn 1: {first}");

    let second = agent
        .prompt("What project name did I ask you to remember?")
        .conversation("local-dev-session")
        .await?;
    println!("turn 2: {second}");

    Ok(())
}
