#!/bin/sh

set -eu

root_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
binary_name="herdr-agent-diff"

detect_target() {
    case "$(uname -s):$(uname -m)" in
        Darwin:arm64)
            printf '%s\n' "aarch64-apple-darwin"
            ;;
        Linux:x86_64|Linux:amd64)
            printf '%s\n' "x86_64-unknown-linux-gnu"
            ;;
        Linux:aarch64|Linux:arm64)
            printf '%s\n' "aarch64-unknown-linux-gnu"
            ;;
        *)
            printf '%s\n' "herdr-agent-diff does not support this OS/architecture: $(uname -s) $(uname -m)." >&2
            return 1
            ;;
    esac
}

target_triple=$(detect_target)
binary_path="$root_dir/target/$target_triple/release/$binary_name"

manifest_version=$(awk -F '"' '$1 ~ /^version[[:space:]]*=/ { print $2; exit }' "$root_dir/herdr-plugin.toml")
release_tag="v$manifest_version"
package_name="$binary_name-$release_tag-$target_triple"
archive_name="$package_name.tar.gz"
release_base_url="https://github.com/baotran01/herdr-agent-diff/releases/download/$release_tag"

temp_dir=
staged_binary=
cleanup() {
    if [ -n "${staged_binary:-}" ]; then
        rm -f "$staged_binary"
    fi
    if [ -n "${temp_dir:-}" ]; then
        rm -rf "$temp_dir"
    fi
}
trap cleanup EXIT HUP INT TERM

download_release() {
    command -v curl >/dev/null 2>&1 || return 1
    command -v tar >/dev/null 2>&1 || return 1
    if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
        return 1
    fi

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
    if command -v sha256sum >/dev/null 2>&1; then
        actual_hash=$(sha256sum "$archive_path" | awk '{ print $1 }')
    else
        actual_hash=$(shasum -a 256 "$archive_path" | awk '{ print $1 }')
    fi
    [ -n "$expected_hash" ] && [ "$expected_hash" = "$actual_hash" ] || return 1

    mkdir -p "$(dirname "$binary_path")"
    if ! tar -xzf "$archive_path" -C "$temp_dir" "$package_name/$binary_name"; then
        return 1
    fi
    if [ ! -f "$temp_dir/$package_name/$binary_name" ]; then
        return 1
    fi

    replace_binary "$temp_dir/$package_name/$binary_name" "$binary_path"
}

replace_binary() {
    source_path=$1
    destination_path=$2
    staged_binary="$destination_path.tmp-$$"
    cp "$source_path" "$staged_binary"
    chmod 755 "$staged_binary"
    mv -f "$staged_binary" "$destination_path"
    staged_binary=
}

find_cargo() {
    if command -v cargo >/dev/null 2>&1; then
        command -v cargo
        return 0
    fi

    for candidate in \
        "${HOME:-}/.cargo/bin/cargo" \
        "/usr/local/bin/cargo" \
        "/usr/bin/cargo" \
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
Publish the matching $target_triple release or install Rust/Cargo, then retry
the Herdr plugin install.
$(if [ -x "$binary_path" ]; then printf '%s\n' "The existing installed binary was left unchanged."; fi)
EOF
exit 1
