#!/bin/sh
# Project-owned launchd wrapper: KeepAlive.SuccessfulExit=false.
# It runs exactly one child. launchd, not this script, handles transient restart.
set -u
if [ "$#" -ne 2 ]; then
  echo '用法：launchd-run.sh <reference-client绝对路径> <配置绝对路径>' >&2
  exit 2
fi
"$1" --config "$2" run &
child=$!
stopping=0
stop_child() {
  stopping=1
  kill -TERM "$child" 2>/dev/null || true
}
trap stop_child INT TERM HUP
# wait may be interrupted by a trapped signal before the child has persisted state.
while :; do
  wait "$child"
  code=$?
  if kill -0 "$child" 2>/dev/null; then continue; fi
  break
done
if [ "$stopping" -eq 1 ]; then exit 0; fi
case "$code" in
  4|5) exit 0 ;;
  *) exit "$code" ;;
esac
