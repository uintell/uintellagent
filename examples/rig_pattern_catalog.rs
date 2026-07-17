const PATTERNS: &[(&str, &str, &str)] = &[
    ("agent", "active", "smallest prompt/response agent"),
    (
        "agent_with_tools",
        "active",
        "typed tool definitions and tool registry",
    ),
    (
        "manual_tool_calls",
        "active",
        "explicit tool-call execution",
    ),
    (
        "agent_run_stepping",
        "active",
        "visible step-by-step agent runtime",
    ),
    (
        "agent_with_human_in_the_loop",
        "active",
        "interactive tool approval",
    ),
    (
        "agent_with_approval_policy",
        "active",
        "policy-driven tool gating",
    ),
    (
        "agent_with_durable_approval",
        "active",
        "resumable approval state",
    ),
    (
        "agent_with_memory",
        "active",
        "conversation and graph memory",
    ),
    (
        "agent_with_memory_streaming",
        "active",
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
        "active",
        "planner/orchestrator workflow",
    ),
    (
        "agent_parallelization",
        "planned",
        "parallel specialist agents",
    ),
    (
        "agent_routing",
        "active",
        "route prompts to specialist flows",
    ),
    ("agent_prompt_chaining", "active", "multi-stage task chains"),
    (
        "multi_agent",
        "active",
        "planner/coder/reviewer/tester roles",
    ),
    (
        "agent_with_agent_tool",
        "planned",
        "agent-as-tool composition",
    ),
    ("reasoning_loop", "active", "explicit planner/executor loop"),
    ("rmcp", "planned", "MCP tool integration"),
    (
        "rig-surrealdb/vector_search_surreal",
        "partial",
        "SurrealDB vector memory",
    ),
];

fn main() {
    println!("Rig pattern status in UIntellAgent:\n");
    for (name, status, purpose) in PATTERNS {
        println!("{name:36} {status:8} {purpose}");
    }
}
