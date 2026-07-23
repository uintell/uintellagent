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
Set UINTELL_INSTALL_FISH=0 to skip Fish completion and function installation.
EOF
}

install_fish_support() {
    local binary="$1"
    [[ "${UINTELL_INSTALL_FISH:-1}" != "0" ]] || return 0

    local config_home="${XDG_CONFIG_HOME:-${HOME:?HOME must be set}/.config}"
    local completion_dir="$config_home/fish/completions"
    local function_dir="$config_home/fish/functions"
    local completion_path="$completion_dir/uintell-agent.fish"
    local function_path="$function_dir/ua.fish"
    local completion_staged function_staged
    local completion_installed=0
    local function_installed=0

    if ! install -d -m 0755 "$completion_dir" "$function_dir"; then
        warn "could not create Fish integration directories"
        return 0
    fi
    if ! completion_staged="$(mktemp "$completion_dir/.uintell-agent.XXXXXX")"; then
        warn "could not stage Fish completions"
        return 0
    fi
    if "$binary" completions fish >"$completion_staged"; then
        if chmod 0644 "$completion_staged" && mv -- "$completion_staged" "$completion_path"; then
            completion_installed=1
        else
            rm -f -- "$completion_staged"
            warn "could not publish Fish completions"
        fi
    else
        rm -f -- "$completion_staged"
        warn "installed binary could not generate Fish completions"
    fi

    if ! function_staged="$(mktemp "$function_dir/.ua.XXXXXX")"; then
        warn "could not stage the Fish ua function"
        return 0
    fi
    if "$binary" fish-init >"$function_staged"; then
        if chmod 0644 "$function_staged" && mv -- "$function_staged" "$function_path"; then
            function_installed=1
        else
            rm -f -- "$function_staged"
            warn "could not publish the Fish ua function"
        fi
    else
        rm -f -- "$function_staged"
        warn "installed binary could not generate the Fish ua function"
    fi

    if [[ "$completion_installed" -eq 1 && "$function_installed" -eq 1 ]]; then
        printf 'Installed Fish completions and the `ua` function under %s/fish\n' "$config_home"
    elif [[ "$completion_installed" -eq 1 || "$function_installed" -eq 1 ]]; then
        warn "Fish integration was only partially installed under $config_home/fish"
    fi
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

install -d -m 0755 "$install_dir" || fail "could not create install directory: $install_dir"

if [[ "$action" == "--rollback" ]]; then
    [[ -x "$backup" ]] || fail "no previous UIntell Agent binary is available at $backup"
    "$backup" --version >/dev/null || fail "the previous binary failed its version check"
    swap="$(mktemp "$install_dir/.uintell-agent.rollback.XXXXXX")" || fail "could not create rollback staging file"
    rm -f -- "$swap" || fail "could not prepare rollback staging path"
    if [[ -e "$destination" ]]; then
        mv -- "$destination" "$swap" || fail "could not stage the installed binary for rollback"
    fi
    if ! mv -- "$backup" "$destination"; then
        if [[ -e "$swap" ]] && ! mv -- "$swap" "$destination"; then
            fail "could not restore either binary; the original remains at $swap"
        fi
        fail "could not restore the previous binary"
    fi
    if [[ -e "$swap" ]]; then
        if ! mv -- "$swap" "$backup"; then
            warn "rollback succeeded, but the replaced binary remains at $swap"
        fi
    fi
    printf 'Rolled back %s\n' "$destination"
    "$destination" --version
    install_fish_support "$destination"
    exit 0
fi

[[ -f "$source_binary" ]] || fail "uintell-agent must be next to install.sh"
[[ -x "$source_binary" ]] || fail "packaged uintell-agent is not executable"
"$source_binary" --version >/dev/null || fail "packaged uintell-agent failed its version check"

staged="$(mktemp "$install_dir/.uintell-agent.install.XXXXXX")" || fail "could not create installation staging file"
backup_staged=""
cleanup() {
    rm -f -- "${staged:-}" "${backup_staged:-}"
}
trap cleanup EXIT
install -m 0755 "$source_binary" "$staged" || fail "could not copy the packaged binary into staging"
"$staged" --version >/dev/null || fail "staged uintell-agent failed its version check"

if [[ -e "$destination" ]]; then
    backup_staged="$(mktemp "$install_dir/.uintell-agent.previous.XXXXXX")" || fail "could not create backup staging file"
    cp -p -- "$destination" "$backup_staged" || fail "could not back up the installed binary"
    mv -- "$backup_staged" "$backup" || fail "could not publish the previous binary backup"
    backup_staged=""
fi
mv -- "$staged" "$destination" || fail "could not publish the staged binary"
staged=""

printf 'Installed %s\n' "$destination"
"$destination" --version
install_fish_support "$destination"

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
