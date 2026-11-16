#!/bin/sh
# Read-only local readiness; --production exits 2 when existing M3 gates remain.
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exec "$repo/scripts/run-ops.sh" delivery release-check "$@"
