#!/bin/sh

set -eu

root_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target_triple="aarch64-apple-darwin"
binary_name="herdr-agent-diff"
binary_path="$root_dir/target/$target_triple/release/$binary_name"

manifest_version=$(awk -F '"' '$1 ~ /^version[[:space:]]*=/ { print $2; exit }' "$root_dir/herdr-plugin.toml")
release_tag="v$manifest_version"
package_name="$binary_name-$release_tag-$target_triple"
archive_name="$package_name.tar.gz"
release_base_url="https://github.com/baotran01/herdr-agent-diff/releases/download/$release_tag"

temp_dir=
cleanup() {
    if [ -n "${temp_dir:-}" ]; then
        rm -rf "$temp_dir"
    fi
}
trap cleanup EXIT HUP INT TERM

download_release() {
    command -v curl >/dev/null 2>&1 || return 1
    command -v shasum >/dev/null 2>&1 || return 1
    command -v tar >/dev/null 2>&1 || return 1

    temp_dir=$(mktemp -d "${TMPDIR:-/tmp}/herdr-agent-diff.XXXXXX") || return 1
    archive_path="$temp_dir/$archive_name"
    checksum_path="$temp_dir/$archive_name.sha256"

    if ! curl --fail --silent --show-error --location --retry 2 \
        --connect-timeout 10 --max-time 120 \
        "$release_base_url/$archive_name" --output "$archive_path" \
        2>/dev/null; then
        return 1
    fi
    if ! curl --fail --silent --show-error --location --retry 2 \
        --connect-timeout 10 --max-time 120 \
        "$release_base_url/$archive_name.sha256" --output "$checksum_path" \
        2>/dev/null; then
        return 1
    fi

    expected_hash=$(awk 'NR == 1 { print $1; exit }' "$checksum_path")
    actual_hash=$(shasum -a 256 "$archive_path" | awk '{ print $1 }')
    [ -n "$expected_hash" ] && [ "$expected_hash" = "$actual_hash" ] || return 1

    mkdir -p "$(dirname "$binary_path")"
    if ! tar -xzf "$archive_path" -C "$temp_dir" "$package_name/$binary_name"; then
        return 1
    fi
    if [ ! -f "$temp_dir/$package_name/$binary_name" ]; then
        return 1
    fi

    cp "$temp_dir/$package_name/$binary_name" "$binary_path"
    chmod 755 "$binary_path"
}

find_cargo() {
    if command -v cargo >/dev/null 2>&1; then
        command -v cargo
        return 0
    fi

    for candidate in \
        "${HOME:-}/.cargo/bin/cargo" \
        "/opt/homebrew/opt/rustup/bin/cargo" \
        "/usr/local/opt/rustup/bin/cargo" \
        "/opt/homebrew/bin/cargo" \
        "/usr/local/bin/cargo"; do
        if [ -x "$candidate" ]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done

    return 1
}

if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
    printf '%s\n' "herdr-agent-diff requires macOS on Apple silicon (aarch64-apple-darwin)." >&2
    exit 1
fi

# Marketplace installs use this path, so a Rust toolchain is not required once
# the matching GitHub release exists. The checksum is verified before copying.
if download_release; then
    exit 0
fi

if cargo_path=$(find_cargo); then
    # GUI-launched Herdr may not inherit the shell PATH. Rustup's cargo and
    # rustc proxies must be visible together for the fallback to work.
    export PATH="$(dirname "$cargo_path"):/opt/homebrew/opt/rustup/bin:/opt/homebrew/bin:/usr/local/opt/rustup/bin:/usr/local/bin:${PATH:-}"
    cd "$root_dir"
    exec "$cargo_path" build --locked --release --target "$target_triple"
fi

cat >&2 <<EOF
Could not install $binary_name.

The prebuilt release $release_tag is unavailable, and Cargo was not found.
Publish the matching GitHub release or install Rust/Cargo, then retry the Herdr
plugin install.
EOF
exit 1
