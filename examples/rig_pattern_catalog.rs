const PATTERNS: &[(&str, &str)] = &[
    ("agent", "smallest prompt/response agent"),
    (
        "agent_with_tools",
        "typed tool definitions and tool registry",
    ),
    ("manual_tool_calls", "explicit tool-call execution"),
    ("agent_run_stepping", "visible step-by-step agent runtime"),
    ("agent_with_human_in_the_loop", "interactive tool approval"),
    ("agent_with_approval_policy", "policy-driven tool gating"),
    ("agent_with_durable_approval", "resumable approval state"),
    ("agent_with_memory", "conversation memory"),
    ("agent_with_memory_streaming", "streamed output with memory"),
    ("rag", "document/codebase retrieval"),
    ("rag_dynamic_tools", "embedding-selected tools"),
    (
        "rag_dynamic_tools_multi_turn",
        "dynamic tools across a session",
    ),
    ("agent_orchestrator", "planner/orchestrator workflow"),
    ("agent_parallelization", "parallel specialist agents"),
    ("agent_routing", "route prompts to specialist flows"),
    ("agent_prompt_chaining", "multi-stage task chains"),
    ("multi_agent", "planner/coder/reviewer/tester architecture"),
    ("agent_with_agent_tool", "agent-as-tool composition"),
    ("reasoning_loop", "explicit planner/executor loop"),
    ("rmcp", "MCP tool integration"),
    (
        "rig-surrealdb/vector_search_surreal",
        "SurrealDB vector memory",
    ),
];

fn main() {
    println!("Rig patterns integrated into UIntellAgent examples:\n");
    for (name, purpose) in PATTERNS {
        println!("{name:36} {purpose}");
    }
}
