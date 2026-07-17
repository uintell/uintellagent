# UIntellAgent Rig Integration Examples

These examples adapt the local Rig examples in `rig/examples` into UIntellAgent
patterns. They are meant to be small, readable entry points for building the
real agent features inside `src/`.

Run examples with:

```sh
cargo run --example rig_pattern_catalog
cargo run --example rig_tool_calling
cargo run --example rig_visible_steps
cargo run --example rig_approval_policy
cargo run --example rig_memory
cargo run --example rig_all_in_one
```

Provider examples use DeepSeek by default, matching UIntellAgent. Set:

```sh
export DEEPSEEK_API_KEY="..."
```

## Current UIntell Examples

- `rig_tool_calling.rs`: Rig `Tool` pattern adapted for UIntell-style tools.
- `rig_visible_steps.rs`: manual `AgentRun` stepping so every model/tool step is visible.
- `rig_approval_policy.rs`: human approval hook for side-effecting tools.
- `rig_memory.rs`: conversation memory using Rig's memory backend.
- `rig_pattern_catalog.rs`: maps the full Rig examples tree to UIntellAgent features.
- `rig_all_in_one.rs`: one command that combines catalog, tool calling, visible steps,
  approval hooks, memory, routing, chaining, multi-agent planning, and graph/RAG notes.

## Rig Examples Mapped To UIntellAgent

| Rig example | UIntellAgent use |
| --- | --- |
| `agent` | smallest prompt/response agent |
| `agent_with_tools` | UIntell tool registry and tool definitions |
| `manual_tool_calls` | explicit tool execution without hiding the loop |
| `agent_run_stepping` | visible step-by-step agent runtime |
| `agent_with_human_in_the_loop` | approval UI for risky actions |
| `agent_with_approval_policy` | policy-driven tool gating |
| `agent_with_durable_approval` | resumable approval after restart |
| `agent_with_memory` | conversation memory |
| `agent_with_memory_streaming` | live TUI memory and streamed responses |
| `multi_turn_agent` | stateful coding sessions |
| `multi_turn_agent_extended` | richer chat/session orchestration |
| `agent_with_context` | prompt context injection |
| `agent_with_default_max_turns` | bounded execution |
| `agent_stream_chat` | TUI streaming output |
| `rag` | codebase/document retrieval |
| `rag_dynamic_tools` | choose relevant tools from embeddings |
| `rag_dynamic_tools_multi_turn` | retrieval plus multi-turn state |
| `custom_vector_store` | UIntell custom memory index |
| `vector_search` | in-memory vector search |
| `vector_search_ollama` | local embeddings |
| `vector_search_cohere` | provider-specific embeddings |
| `rag_ollama` | fully local RAG |
| `agent_with_loaders` | load files/PDF/text into context |
| `pdf_agent` | PDF code/docs assistant |
| `extractor` | structured output |
| `multi_extract` | batch structured extraction |
| `sentiment_classifier` | classifier agents |
| `chain` | prompt chains |
| `agent_prompt_chaining` | staged task pipelines |
| `agent_routing` | route requests to specialist modes |
| `agent_parallelization` | parallel specialist agents |
| `agent_orchestrator` | planner/orchestrator flow |
| `agent_evaluator_optimizer` | generate/review/improve loop |
| `multi_agent` | planner/coder/reviewer/tester architecture |
| `agent_with_agent_tool` | agent-as-tool composition |
| `reasoning_loop` | explicit planner/executor loop |
| `debate` | multi-agent critique |
| `complex_agentic_loop_claude` | provider-specific advanced loop |
| `calculator_chatbot` | focused tool chatbot |
| `enum_dispatch` | typed dispatch patterns |
| `request_hook` | provider request interception |
| `reqwest_middleware` | HTTP client middleware |
| `rmcp` | MCP integration |
| `discord_bot` | external chat surface |
| `transcription` | audio input workflow |
| `gemini_*` | provider-specific multimodal/recovery/image/video patterns |
| `openai_*_otel` and `agent_with_tools_otel` | tracing and observability |
| `crates/rig-surrealdb/examples` | SurrealDB vector memory for graph/RAG |
