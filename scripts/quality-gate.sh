#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo"
export CARGO_HOME="${CARGO_HOME:-$repo/.local/cargo}"
export PATH="$repo/.local/tools:$PATH"

if [ "${TOLIGHT_TEST_DB_READY:-}" != 1 ]; then
  exec "$repo/scripts/with-test-db.sh" "$repo/scripts/quality-gate.sh"
fi
export PATH="$repo/.local/sqlx-cli/bin:$PATH"

if [ -n "${PYTHON:-}" ]; then
  python=$PYTHON
elif [ -x "$repo/.local/venv/bin/python" ]; then
  python=$repo/.local/venv/bin/python
else
  python=python3
fi

npm run check:web
npm run build:web
cargo fmt --all -- --check
cargo sqlx migrate run --source crates/tolight-coordinator-store/migrations
SQLX_OFFLINE=true cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
SQLX_OFFLINE=true cargo test --locked --workspace --all-targets
# The lifecycle harness normally uses Cargo's prebuilt CLI: recursively building
# the same target while cargo test owns its artifact lock would deadlock. Once
# Cargo has exited, rerun the real lifecycle in source-build mode as well.
local_dev_artifacts=$(mktemp "$repo/.local/logs/local-dev-rust-artifacts.XXXXXX")
trap 'rm -f -- "$local_dev_artifacts"' 0
SQLX_OFFLINE=true cargo test --locked -p tolight-cli --test local_dev --no-run --message-format=json > "$local_dev_artifacts"
local_dev_executable=$(scripts/run-ops.sh cargo-test-executable --artifacts "$local_dev_artifacts" --target local_dev)
rm -f -- "$local_dev_artifacts"
trap - 0
TOLIGHT_LOCAL_DEV_TEST_DEFAULT_BUILD=1 "$local_dev_executable" --exact owned_environment_survives_restart_and_produces_both_real_backups
(cd crates/tolight-coordinator-store && SQLX_OFFLINE=false cargo sqlx prepare --check -- --all-targets --all-features)
cargo deny --locked check
npm run test:web
# Deliberately independent implementation: interoperability signature vectors only.
"$python" examples/python/check_vectors.py
