#!/usr/bin/env bash
set -euo pipefail

binary=${1:-target/release/kurama}
bundle=$(mktemp)
trap 'rm -f "$bundle"' EXIT

cargo build --locked --release -p kurama-cli
test -x "$binary"
"$binary" --internal-print-prompt-bundle > "$bundle"
test -s "$bundle"

cargo run \
  --quiet \
  --locked \
  --release \
  --manifest-path tools/prompt-budget/Cargo.toml \
  < "$bundle"
