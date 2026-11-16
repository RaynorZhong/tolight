#!/bin/sh
# Source-only local rehearsal; never commit, tag or publish the user's repository.
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo"
umask 077
export CARGO_HOME="${CARGO_HOME:-$repo/.local/cargo}"
exec "$repo/scripts/run-ops.sh" delivery source-rehearsal "$@"
