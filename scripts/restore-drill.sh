#!/bin/sh
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
mode=restore-drill
if [ "${1:-}" = --mode ]; then
    [ "$#" -ge 2 ] || { echo '--mode requires logical or pitr' >&2; exit 2; }
    case "$2" in
        pitr) mode=pitr-drill ;;
        logical) ;;
        *) echo '--mode requires logical or pitr' >&2; exit 2 ;;
    esac
    shift 2
fi
exec "$repo/scripts/run-ops.sh" storage "$mode" "$@"
