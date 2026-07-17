pub mod browser;
pub mod code;
pub mod file;
pub mod file_search;
pub mod graph;
pub mod search;
pub mod terminal;

// Re-exports
pub use crate::mesh::ProviderMesh;
pub use browser::Browser;
pub use code::CodeExec;
pub use file::{FileRead, FileWrite};
pub use file_search::FileSearch;
pub use graph::{GraphContext, GraphEdit, GraphForget, GraphQuery, GraphStore};
pub use search::WebSearch;
pub use terminal::Terminal;

use rig_core::tool::ToolDyn;

pub struct ToolCatalogEntry {
    pub name: &'static str,
    pub description: &'static str,
    pub example: &'static str,
}

pub const CATALOG: &[ToolCatalogEntry] = &[
    ToolCatalogEntry {
        name: "terminal",
        description: "Run a command in the persistent project shell.",
        example: r#"{"command":"pwd"}"#,
    },
    ToolCatalogEntry {
        name: "file_read",
        description: "Read a text file with line numbers.",
        example: r#"{"path":"Cargo.toml","limit":40}"#,
    },
    ToolCatalogEntry {
        name: "file_write",
        description: "Create or overwrite a workspace file.",
        example: r#"{"path":"/tmp/uintell-tool-test.txt","content":"hello"}"#,
    },
    ToolCatalogEntry {
        name: "file_search",
        description: "Search project files with ripgrep.",
        example: r#"{"pattern":"graph_context","path":"src","file_glob":"*.rs"}"#,
    },
    ToolCatalogEntry {
        name: "browser",
        description: "Fetch a web page as text or HTML.",
        example: r#"{"url":"https://rig.rs/","text_only":true}"#,
    },
    ToolCatalogEntry {
        name: "web_search",
        description: "Search the web through DuckDuckGo.",
        example: r#"{"query":"Rust Rig framework","limit":5}"#,
    },
    ToolCatalogEntry {
        name: "code_exec",
        description: "Execute sandboxed Python, Bash, Rust, or Node.js code.",
        example: r#"{"language":"python","code":"print(2 + 2)"}"#,
    },
    ToolCatalogEntry {
        name: "graph_store",
        description: "Store a persistent fact in SurrealDB memory.",
        example: r#"{"fact_type":"memory","content":"UIntell tool console works"}"#,
    },
    ToolCatalogEntry {
        name: "graph_query",
        description: "Search persistent graph-memory facts.",
        example: r#"{"query":"UIntell","limit":10}"#,
    },
    ToolCatalogEntry {
        name: "graph_context",
        description: "Load recent high-confidence memory context.",
        example: r#"{"limit":20}"#,
    },
    ToolCatalogEntry {
        name: "graph_edit",
        description: "Edit a SurrealDB fact by record ID.",
        example: r#"{"fact_id":"fact:replace_me","content":"updated text"}"#,
    },
    ToolCatalogEntry {
        name: "graph_forget",
        description: "Permanently delete a fact by record ID.",
        example: r#"{"fact_id":"fact:replace_me"}"#,
    },
    ToolCatalogEntry {
        name: "provider_mesh",
        description: "Race multiple configured model providers.",
        example: r#"{"prompt":"Answer with one sentence","providers":["deepseek"]}"#,
    },
];

pub async fn execute_named(name: &str, args: &str) -> Result<String, String> {
    let result = match name {
        "terminal" => ToolDyn::call(&Terminal, args.to_string()).await,
        "file_read" => ToolDyn::call(&FileRead, args.to_string()).await,
        "file_write" => ToolDyn::call(&FileWrite, args.to_string()).await,
        "file_search" => ToolDyn::call(&FileSearch, args.to_string()).await,
        "browser" => ToolDyn::call(&Browser, args.to_string()).await,
        "web_search" => ToolDyn::call(&WebSearch, args.to_string()).await,
        "code_exec" => ToolDyn::call(&CodeExec, args.to_string()).await,
        "graph_store" => ToolDyn::call(&GraphStore, args.to_string()).await,
        "graph_query" => ToolDyn::call(&GraphQuery, args.to_string()).await,
        "graph_context" => ToolDyn::call(&GraphContext, args.to_string()).await,
        "graph_edit" => ToolDyn::call(&GraphEdit, args.to_string()).await,
        "graph_forget" => ToolDyn::call(&GraphForget, args.to_string()).await,
        "provider_mesh" => ToolDyn::call(&ProviderMesh, args.to_string()).await,
        _ => return Err(format!("unknown tool: {name}")),
    };
    result.map_err(|error| error.to_string())
}
