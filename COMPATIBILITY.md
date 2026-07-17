# Compatibility Policy

UIntell Agent follows Semantic Versioning beginning with `1.0.0`.

## Stable 1.x Contract

Within the `1.x` series:

- Existing CLI commands and documented flags remain available. New flags may be
  added with backward-compatible defaults.
- The `/health`, `/ready`, and `/chat` gateway endpoints remain compatible.
  JSON responses may gain fields, but documented fields and error codes are not
  removed or redefined in a minor or patch release.
- Permission configuration version 1, task-run checkpoints version 1, workspace
  state version 1, graph snapshots version 1, graph schema version 1, and local
  instruction-skill manifests version 1 remain readable.
- Graph schema changes are additive and initialized automatically. UIntell Agent
  refuses to open a graph created by a newer, unsupported schema instead of
  silently downgrading it.
- Deprecations are documented before removal. Breaking changes require a new
  major version.

The `v0.3.0-alpha.1` persisted formats already use version 1 and are accepted by
`1.0.0`. On first load, its unversioned permission file is migrated to version 1
and machine-specific development paths are removed.

## Supported Platform

The official `1.0.0` binary supports x86-64 glibc-based Linux and is built on
Ubuntu 22.04. Other platforms may build from source but are not part of the
`1.0.0` binary support contract.

Runtime requirements:

- SurrealDB CLI in `PATH` for automatically managed graph memory.
- `/usr/bin/bwrap` for sandboxed code execution.
- `DEEPSEEK_API_KEY`, or a running Ollama instance selected with `--ollama`.
- Rust 1.94 or newer only when building from source or executing Rust code.
- `rust-analyzer` is optional and enables Rust code intelligence.

## Upgrade And Rollback

Before a major upgrade, export important graph datasets and keep a copy of
`~/.uintell`. The installer validates a staged binary before atomically replacing
the installed binary and preserves one previous binary. From an extracted
release package, run:

```bash
./install.sh --rollback
```

Binary rollback does not reverse data migrations. The `1.x` policy therefore
keeps persisted format versions backward-compatible for the entire major series.
