# Changelog

All notable changes are documented here. This project follows Semantic
Versioning from `1.0.0` onward.

## 1.0.1 - 2026-07-23

### Shell And Input

- Added generated Fish completions and the `ua` Fish function, with automatic
  per-user installation and a package-manager opt-out.
- Added private, project-scoped prompt history with TUI recall, visible
  autosuggestions, slash-command completion, CLI inspection, and clearing.
- Added conservative secret detection so likely credentials, private keys,
  recovery phrases, and Nostr secret keys are not written to prompt history.

## 1.0.0 - 2026-07-17

### Stable Contracts

- Established backward-compatible CLI, gateway, permission, checkpoint,
  workspace, graph snapshot, and graph schema contracts for the `1.x` series.
- Added explicit graph schema metadata and refusal of unsupported future schema
  versions.
- Added migration of the alpha permission configuration to stable version 1.

### Security

- Replaced machine-specific permission defaults with current-workspace policy.
- Added symlink-aware path checks, argv-level shell authorization, command-specific
  confirmation, exact domain matching, and fail-closed configuration loading.
- Removed provider-key fallback from gateway authentication and added bounded,
  constant-time API-key comparison.
- Bounded gateway request bodies, request IDs, sessions, history, queue time, and
  returned proper HTTP failure statuses.
- Moved terminal output files into private random directories, independently
  drained bounded stdout/stderr, and made truncation safe for arbitrary bytes.
- Re-authorized browser redirect targets and bounded HTTP, file, code, and
  terminal capture sizes.
- Made provider-mesh calls fail closed through the shared permission engine.
- Removed the built-in model safety-bypass mode.

### Distribution

- Added atomic installation, previous-binary preservation, and rollback.
- Added RustSec audits, minimum-Rust CI, Dependabot configuration, pinned action
  revisions, and signed GitHub build provenance.
- Removed an unused SurrealDB SDK dependency and upgraded Ratatui, Crossterm, and
  the textarea component, eliminating all RustSec findings in the release lockfile.
- Added the stable HTTP gateway reference, including limits and error contracts.

### Skills

- Replaced the metadata-only future-Wasm placeholder with functional,
  explicitly selected Markdown instruction skills.
- Added traversal-safe names and entrypoints, private atomic creation, bounded
  metadata and instruction loading, and `--skill <name>` composition.

## 0.3.0-alpha.1 - 2026-07-17

- First public alpha with the unified TUI, tools, graph memory, editor, durable
  runs, provider integration, and HTTP gateway.
