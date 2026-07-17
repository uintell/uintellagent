# Security Model

UIntell Agent is tool-capable software. Treat it as privileged automation.

## Supported Versions

Security fixes are provided for the latest `1.x` release. Upgrade to the newest
patch before reporting an issue that may already be fixed.

Report vulnerabilities privately through the repository's GitHub Security
Advisories page. Do not open a public issue for an undisclosed vulnerability.

## Defaults

- Gateway auth fails closed when `UINTELL_API_KEY` is not set.
- Gateway auth never falls back to a model-provider credential.
- Tool permissions are enforced in the hook and again inside tools.
- Confirmation approvals are single-use.
- Code execution requires `bubblewrap` unless explicitly overridden.
- File reads deny common secret paths such as SSH keys and private key files.
- Permission files with invalid or unsupported versions fail closed.
- Workspace shell auto-approval is limited to argument-validated commands;
  expansion, redirection, interpreters, repository code, and filesystem
  inspection require confirmation.
- Release archives are checksummed and receive signed GitHub build provenance.

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

## Release Verification

Verify `SHA256SUMS` before installation. GitHub CLI users can also verify signed
build provenance:

```bash
gh attestation verify uintell-agent-1.0.0-x86_64-unknown-linux-gnu.tar.gz \
  --repo uintell/uintellagent
```

## Recommended Production Posture

- Run the agent as a non-root user.
- Use a private network binding or reverse proxy.
- Keep code execution sandboxed.
- Use SurrealDB credentials with the minimum required permissions.
- Review audit logs for shell, file, network, code, and graph-memory tools.
