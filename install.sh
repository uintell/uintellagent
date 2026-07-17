#!/usr/bin/env bash
set -euo pipefail

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

warn() {
    printf 'warning: %s\n' "$*" >&2
}

case "$(uname -s)" in
    Linux) ;;
    *) fail "this release supports Linux only" ;;
esac

case "$(uname -m)" in
    x86_64 | amd64) ;;
    *) fail "this release supports x86-64 only" ;;
esac

script_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source_binary="$script_dir/uintell-agent"
install_dir="${UINTELL_INSTALL_DIR:-${HOME:?HOME must be set}/.local/bin}"
destination="$install_dir/uintell-agent"

[[ -f "$source_binary" ]] || fail "uintell-agent must be next to install.sh"

install -d "$install_dir"
install -m 0755 "$source_binary" "$destination"

printf 'Installed %s\n' "$destination"
"$destination" --version

case ":${PATH:-}:" in
    *":$install_dir:"*) ;;
    *) printf 'Add %s to PATH before launching UIntell Agent.\n' "$install_dir" ;;
esac

[[ -x /usr/bin/bwrap ]] || warn "Bubblewrap is missing; sandboxed code execution will be unavailable"
command -v surreal >/dev/null 2>&1 || warn "SurrealDB is missing; graph memory cannot auto-start"
command -v python3 >/dev/null 2>&1 || warn "Python code execution will be unavailable"
command -v node >/dev/null 2>&1 || warn "Node.js code execution will be unavailable"
command -v rustc >/dev/null 2>&1 || warn "Rust code execution will be unavailable"

printf 'Run `uintell-agent doctor` to verify the complete runtime.\n'
