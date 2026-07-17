# UIntell Agent

Rust-native AI agent with a TUI, HTTP gateway, tool execution, provider mesh, and SurrealDB graph memory.

## Install

UIntell Agent 1.0 supports glibc-based Linux on x86-64. Download and verify the
stable release archive:

```bash
version=1.0.0
asset="uintell-agent-${version}-x86_64-unknown-linux-gnu"
base_url="https://github.com/uintell/uintellagent/releases/download/v${version}"

curl --fail --location --remote-name "${base_url}/${asset}.tar.gz"
curl --fail --location --remote-name "${base_url}/SHA256SUMS"
sha256sum --ignore-missing --check SHA256SUMS
tar -xzf "${asset}.tar.gz"
cd "$asset"
./install.sh
export PATH="$HOME/.local/bin:$PATH"
uintell-agent doctor
uintell-agent --tui
```

The installer copies the binary to `~/.local/bin` by default. Set
`UINTELL_INSTALL_DIR` to choose another location. Release files and notes are
available on the [Releases page](https://github.com/uintell/uintellagent/releases).
When upgrading, the installer preserves the previous binary as
`uintell-agent.previous`. Run the extracted package's `./install.sh --rollback`
with the same `UINTELL_INSTALL_DIR` to swap back.

GitHub Actions signs the build provenance for both release artifacts. With the
GitHub CLI installed, verify that the downloaded archive came from this
repository:

```bash
gh attestation verify "${asset}.tar.gz" --repo uintell/uintellagent
```

Graph memory requires the `surreal` CLI in `PATH`. Sandboxed code execution
requires `/usr/bin/bwrap`. DeepSeek requires `DEEPSEEK_API_KEY`; local models
can be selected with `--ollama`.

## Build From Source

Rust 1.94 or newer is required:

```bash
git clone https://github.com/uintell/uintellagent.git
cd uintellagent
cargo build --release --locked
./target/release/uintell-agent --version
```

## Safety Baseline

- Tool calls pass through a shared permission engine.
- Dangerous actions require confirmation unless pre-approved.
- Shell sessions preserve state and capture real exit codes.
- Code execution runs through `bubblewrap` by default.
- HTTP gateway requires `UINTELL_API_KEY` for `/ready` and `/chat`.
- Provider credentials are never accepted as gateway authentication keys.
- Graph memory validates record IDs and serializes user text before building SurrealQL.
- Release artifacts have SHA-256 checksums and signed build provenance.

See [SECURITY.md](SECURITY.md) for deployment guidance and
[COMPATIBILITY.md](COMPATIBILITY.md) for the stable `1.x` support contract. The
[gateway reference](docs/GATEWAY.md) documents the authenticated HTTP API,
limits, status codes, and CORS configuration.

## Commands

```bash
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo run -- doctor
```

## Runtime

```bash
export DEEPSEEK_API_KEY=...
export UINTELL_API_KEY=...
cargo run -- --tui
cargo run -- serve --addr 127.0.0.1:3000
```

`cargo run -- doctor` checks provider authentication, SurrealDB and its schema,
tool permissions, workspace writes, Bubblewrap, and every advertised code
runtime. The TUI reports provider health at startup and keeps Memory, Tools,
Editor, and durable run history available when a provider is offline; Chat and
new Runs stay blocked with an actionable reason until the provider is ready.

For local models:

```bash
cargo run -- --ollama --tui
```

The unified TUI workspaces are:

- `Alt+1`: streaming Chat. `Esc` or `Ctrl+G` cancels an active model/tool run.
- `Alt+2`: graph Memory, Explorer, SurrealQL Query, and Analytics.
- `Alt+3`: the complete tool catalog. Use `:run <tool> <json>` and `:run!` for
  calls that require explicit confirmation.
- `Alt+4`: the Vim-style Editor. Agent `file_write` calls refresh the tree and
  open a diff of the changed file without overwriting an unsaved editor buffer.
- `Alt+5`: durable autonomous Runs with persisted checkpoints, tool events,
  verification/review gates, bounded repair loops, cancellation, and resume.

The active workspace, editor file/cursor, selected task run, and graph view are
restored from `~/.uintell/workspace.json` on the next launch.

## Instruction Skills

Create a private local instruction skill, edit its Markdown entrypoint, and
select it explicitly when launching an agent:

```bash
uintell-agent skill-new rust-review "Review Rust changes against project conventions"
$EDITOR ~/.uintell/skills/rust-review/SKILL.md
uintell-agent --skill rust-review --tui
uintell-agent skills
```

Selected skills are appended to the model's system instructions. Skill names
and entrypoints are traversal-safe, each instruction file is limited to 64 KiB,
and at most eight skills can be active at once. Skill directories and files are
created with private permissions. UIntell Agent 1.0 skills are instruction
modules, not executable plugins.

## Durable Coding Runs

Start a run from Chat with `/task <objective>`, or open Runs with `Alt+5` and
use `n`/`:new <objective>`. A run inspects and plans, implements against real
workspace files, verifies the result, performs an independent review, repairs
failed quality gates, and writes a final engineering report. `c` or `Ctrl+G`
cancels at a checkpoint; `r` resumes the selected cancelled, failed, paused, or
pending run. Agent-authored files remain inspectable as diffs in Editor.

Graph-memory writes are read-only by default during autonomous work. Use
`/task --remember <objective>`, `:new --remember <objective>`, or the CLI
`--remember` flag only when the run should be allowed to persist or mutate
knowledge units.

Runs are stored as private, atomic checkpoints under `~/.uintell/runs`. A
crash-interrupted `running` checkpoint is presented as `paused` and can be
resumed without replaying tool calls already recorded in its result ledger.
Only one process can drive a run at a time.

The same engine is available without the TUI:

```bash
cargo run -- task start "implement the requested change and verify it"
cargo run -- task start --remember "implement and remember the design decision"
cargo run -- task list
cargo run -- task show <run-id>
cargo run -- task resume <run-id>
```

## Memory Graph

Press `2` from another workspace, or `Alt+2` globally, to open the complete
graph operations console inside the unified agent TUI. Its `1`-`4` keys switch
between Graph, Explorer, Query, and Analytics. Use `Alt+1`-`Alt+5` to switch
agent workspaces and `q` to return to Chat. Graph jobs and viewport state remain
alive while another agent workspace is open.

## Graph Operations Console

Run `cargo run -- db` only when you want the same console without loading an AI
provider. The embedded and standalone modes both provide a zoomable graph, a
Yazi-style explorer, a safe SurrealQL workbench, and an analytics dashboard.

- Drag, move, pin, zoom, pan, filter, and auto-layout graph nodes.
- Shift-drag a lasso to mark nodes, use `z`/`Z` to fit selected/all visible
  nodes, and use the minimap on larger terminals to track the viewport.
- Mark multiple units for bulk dataset moves or confirmed deletion.
- Create `relates_to` and `proves` edges and inspect both directions.
- Use `:query` for read-only SurrealQL; `:query!` mutations require an
  additional preview and confirmation.
- Use `:export` for a versioned JSON snapshot, `:import`/`:import!` for validated
  preview and merge, and `:repair` for metadata and duplicate-edge cleanup.
  Partial exports record both loaded and total fact counts.
- Destructive UI actions are journaled in SurrealDB and can be restored with
  `U`/`:undo`, then reapplied with `R`/`:redo`.
- Refresh, layout, and SurrealQL run as background jobs with phase/progress
  status. Use `Esc`, `Ctrl+G`, or `:cancel` to cancel and `:retry` to rerun the
  last safe job.
- Use `:load-more [count]`, `:load-all`, or `>` to increase the interactive
  graph window up to 100,000 nodes. The graph uses viewport culling, indexed
  edge lookup, cached analytics, and a virtualized Explorer list.
- Persist filters, pan, and zoom with `:view-save <name>`; restore with
  `:view <name>` and inspect all views with `:views`.

Both entry points use the same reusable graph-console component, state model,
event handlers, repository, validation, deterministic position repair, layout
engine, analytics, and spatial index.

## Configuration

Permissions are loaded from:

```text
~/.uintell/permissions.toml
```

SurrealDB settings:

```bash
export UINTELL_DB_URL=http://127.0.0.1:8000
export UINTELL_DB_USER=root
export UINTELL_DB_PASS=root
```

With the default local URL, `cargo run -- --tui` starts SurrealDB automatically,
waits for readiness, and initializes the schema. Set `UINTELL_DB_AUTOSTART=0`
only when another service manager owns the database process.

Code execution sandbox:

```bash
export UINTELL_CODE_SANDBOX=1
```

`UINTELL_ALLOW_UNSANDBOXED_CODE=1` exists only as an explicit local-development override.

## License

UIntell Agent is distributed under the terms of the [MIT License](LICENSE).
