#!/bin/sh
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
ops_bin=${TOLIGHT_OPS_BIN:-$repo/target/debug/tolight-ops}
if [ ! -x "$ops_bin" ]; then
    printf '%s\n' 'Build the Rust tools first: cargo build --locked -p tolight-cli' >&2
    exit 1
fi
exec "$ops_bin" "$@"
