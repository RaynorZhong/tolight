#!/bin/sh
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exec "$repo/scripts/run-ops.sh" demo m2 "$@"
