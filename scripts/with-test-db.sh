#!/bin/sh
# Run a command against a disposable PostgreSQL 18 cluster, never an existing DB.
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo"
export CARGO_HOME="${CARGO_HOME:-$repo/.local/cargo}"
export PATH="$repo/.local/pg18/bin:$repo/.local/sqlx-cli/bin:$repo/.local/tools:$PATH"
if [ "$#" -eq 0 ]; then echo '用法：scripts/with-test-db.sh <命令> [参数...]' >&2; exit 2; fi
if ! command -v initdb >/dev/null || ! command -v pg_ctl >/dev/null; then
  echo '需要 PostgreSQL 18 的 initdb/pg_ctl/createdb；进入 Nix 环境或先安装开发工具。' >&2
  exit 1
fi
case "$(initdb --version)" in *' 18.'*) ;; *) echo '测试要求 PostgreSQL 主版本 18' >&2; exit 1 ;; esac
mkdir -p .local/logs
test_root=$(mktemp -d "$repo/.local/test-db.XXXXXX")
started=0
cleanup() {
  code=$?
  trap - 0
  if [ "$started" -eq 1 ]; then
    pg_ctl -D "$test_root/data" -m fast -w stop >/dev/null 2>&1 || true
    if pg_ctl -D "$test_root/data" status >/dev/null 2>&1; then
      echo "测试数据库尚在运行，保留目录供处理：$test_root" >&2
      exit 1
    fi
  fi
  if [ -f "$test_root/postgres.log" ]; then cp "$test_root/postgres.log" "$repo/.local/logs/$(basename "$test_root").log"; fi
  rm -rf -- "$test_root"
  exit "$code"
}
trap cleanup 0
trap 'exit 130' INT
trap 'exit 143' TERM HUP
mkdir "$test_root/socket"
initdb -D "$test_root/data" -U tolight --encoding=UTF8 --locale=C --auth-local=trust --auth-host=reject >"$test_root/initdb.log" 2>&1 || {
  cat "$test_root/initdb.log" >&2; exit 1;
}
started=1
pg_ctl -D "$test_root/data" -l "$test_root/postgres.log" -o "-h '' -k '$test_root/socket' -p 5432 -c timezone=UTC" -w start >/dev/null
export PGHOST="$test_root/socket" PGPORT=5432 PGUSER=tolight PGDATABASE=tolight_test
createdb tolight_test
encoded_socket=$(printf %s "$PGHOST" | od -An -v -tx1 | tr -d ' \n' | sed 's/../%&/g')
export DATABASE_URL="postgresql://tolight@localhost:5432/tolight_test?host=$encoded_socket"
export TOLIGHT_TEST_DB_READY=1
"$@"
