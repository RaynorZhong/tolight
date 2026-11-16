#!/bin/sh
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
umask 077
exec "$repo/scripts/run-ops.sh" runtime verify-public "$@"
