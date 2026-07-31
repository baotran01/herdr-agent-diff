#!/bin/sh

set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
root_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)

case "$(uname -s):$(uname -m)" in
    Darwin:arm64)
        target_triple="aarch64-apple-darwin"
        ;;
    Linux:x86_64|Linux:amd64)
        target_triple="x86_64-unknown-linux-gnu"
        ;;
    Linux:aarch64|Linux:arm64)
        target_triple="aarch64-unknown-linux-gnu"
        ;;
    *)
        printf '%s\n' "herdr-agent-diff does not support this OS/architecture: $(uname -s) $(uname -m)" >&2
        exit 1
        ;;
esac

binary="$root_dir/target/$target_triple/release/herdr-agent-diff"
if [ ! -x "$binary" ]; then
    printf '%s\n' "herdr-agent-diff binary is missing: $binary" >&2
    printf '%s\n' "Run the plugin build step again or install the matching release." >&2
    exit 1
fi

exec "$binary" "$@"
