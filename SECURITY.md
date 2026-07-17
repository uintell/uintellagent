# Security Model

UIntell Agent is tool-capable software. Treat it as privileged automation.

## Defaults

- Gateway auth fails closed when `UINTELL_API_KEY` is not set.
- Tool permissions are enforced in the hook and again inside tools.
- Confirmation approvals are single-use.
- Code execution requires `bubblewrap` unless explicitly overridden.
- File reads deny common secret paths such as SSH keys and private key files.

## Do Not Expose Publicly Without

- TLS termination
- A strong `UINTELL_API_KEY`
- Tight CORS origins
- A locked-down `~/.uintell/permissions.toml`
- Non-root SurrealDB credentials
- External process supervision and log rotation

## Dangerous Overrides

`UINTELL_ALLOW_UNSANDBOXED_CODE=1` disables the fail-closed behavior for code execution. Use it only on disposable local machines.

`UINTELL_CORS_ALLOW_ANY=1` allows any browser origin. Do not use it on public networks.

## Recommended Production Posture

- Run the agent as a non-root user.
- Use a private network binding or reverse proxy.
- Keep code execution sandboxed.
- Use SurrealDB credentials with the minimum required permissions.
- Review audit logs for shell, file, network, code, and graph-memory tools.
