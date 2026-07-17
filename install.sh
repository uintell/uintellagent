#!/usr/bin/env bash
set -euo pipefail

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

warn() {
    printf 'warning: %s\n' "$*" >&2
}

usage() {
    cat <<'EOF'
Usage: ./install.sh [--install | --rollback | --help]

  --install    Install this packaged UIntell Agent binary (default)
  --rollback   Swap the installed binary with its previous version
  --help       Show this help

Set UINTELL_INSTALL_DIR to override the default ~/.local/bin destination.
EOF
}

action="${1:---install}"
[[ $# -le 1 ]] || fail "too many arguments; run ./install.sh --help"
case "$action" in
    --install | --rollback) ;;
    --help | -h)
        usage
        exit 0
        ;;
    *) fail "unknown option: $action; run ./install.sh --help" ;;
esac

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
backup="$install_dir/uintell-agent.previous"

install -d -m 0755 "$install_dir"

if [[ "$action" == "--rollback" ]]; then
    [[ -x "$backup" ]] || fail "no previous UIntell Agent binary is available at $backup"
    "$backup" --version >/dev/null || fail "the previous binary failed its version check"
    swap="$(mktemp "$install_dir/.uintell-agent.rollback.XXXXXX")"
    rm -f -- "$swap"
    if [[ -e "$destination" ]]; then
        mv -- "$destination" "$swap"
    fi
    if ! mv -- "$backup" "$destination"; then
        [[ -e "$swap" ]] && mv -- "$swap" "$destination"
        fail "could not restore the previous binary"
    fi
    if [[ -e "$swap" ]]; then
        mv -- "$swap" "$backup"
    fi
    printf 'Rolled back %s\n' "$destination"
    "$destination" --version
    exit 0
fi

[[ -f "$source_binary" ]] || fail "uintell-agent must be next to install.sh"
[[ -x "$source_binary" ]] || fail "packaged uintell-agent is not executable"
"$source_binary" --version >/dev/null || fail "packaged uintell-agent failed its version check"

staged="$(mktemp "$install_dir/.uintell-agent.install.XXXXXX")"
backup_staged=""
cleanup() {
    rm -f -- "${staged:-}" "${backup_staged:-}"
}
trap cleanup EXIT
install -m 0755 "$source_binary" "$staged"
"$staged" --version >/dev/null || fail "staged uintell-agent failed its version check"

if [[ -e "$destination" ]]; then
    backup_staged="$(mktemp "$install_dir/.uintell-agent.previous.XXXXXX")"
    cp -p -- "$destination" "$backup_staged"
    mv -- "$backup_staged" "$backup"
    backup_staged=""
fi
mv -- "$staged" "$destination"
staged=""

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
