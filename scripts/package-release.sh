#!/usr/bin/env bash
set -euo pipefail

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

[[ $# -ge 1 && $# -le 4 ]] || fail "usage: $0 <vVERSION> [binary] [target] [dist-dir]"

repo_root="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

tag="$1"
binary="${2:-target/release/uintell-agent}"
target="${3:-x86_64-unknown-linux-gnu}"
dist_dir="${4:-dist}"

[[ "$tag" == v* ]] || fail "release tag must start with v"
[[ -x "$binary" ]] || fail "release binary is missing or not executable: $binary"
command -v jq >/dev/null 2>&1 || fail "jq is required to read Cargo metadata"
command -v strip >/dev/null 2>&1 || fail "strip is required to package the binary"

version="${tag#v}"
package_version="$(cargo metadata --locked --no-deps --format-version 1 | jq -r '.packages[0].version')"
[[ "$version" == "$package_version" ]] || fail "tag $tag does not match Cargo version $package_version"

base="uintell-agent-${version}-${target}"
staging_dir="$dist_dir/.staging"
package_dir="$staging_dir/$base"
archive="$dist_dir/$base.tar.gz"
standalone="$dist_dir/$base"
checksums="$dist_dir/SHA256SUMS"

rm -rf -- "$package_dir"
rm -f -- "$archive" "$standalone" "$checksums"
mkdir -p "$package_dir"
mkdir -p "$package_dir/docs"

install -m 0755 "$binary" "$package_dir/uintell-agent"
install -m 0755 install.sh "$package_dir/install.sh"
install -m 0644 README.md LICENSE SECURITY.md COMPATIBILITY.md CHANGELOG.md "$package_dir/"
install -m 0644 docs/GATEWAY.md "$package_dir/docs/"
strip --strip-unneeded "$package_dir/uintell-agent"
install -m 0755 "$package_dir/uintell-agent" "$standalone"

source_date_epoch="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}"
tar \
    --sort=name \
    --mtime="@$source_date_epoch" \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -C "$staging_dir" \
    -czf "$archive" \
    "$base"

(
    cd "$dist_dir"
    sha256sum "$(basename "$standalone")" "$(basename "$archive")" > "$(basename "$checksums")"
)

printf '%s\n' "$standalone" "$archive" "$checksums"
