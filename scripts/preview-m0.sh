#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo"
export CARGO_HOME="${CARGO_HOME:-$repo/.local/cargo}"
npm run build:web
mkdir -p .local
preview=$(mktemp -d "$repo/.local/m0-public.XXXXXX")
trap 'rm -rf -- "$preview"' 0
trap 'exit 130' INT
trap 'exit 143' TERM HUP
cp public/index.html public/app.js public/style.css public/robots.txt "$preview/"
# The sample index is deliberately excluded: the real M0 page is always served.
cp -R tests/fixtures/public-dist-sample/data "$preview/data"
# Keep the shell alive so the isolated demo tree is removed when the server stops.
cargo run --locked -p tolight-cli -- serve-public --dir "$preview" --listen "${1:-127.0.0.1:8080}"
