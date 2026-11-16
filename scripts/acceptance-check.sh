#!/bin/sh
# Read-only local indexing; --final cannot turn local checks into release.
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exec "$repo/scripts/run-ops.sh" delivery acceptance-check "$@"
