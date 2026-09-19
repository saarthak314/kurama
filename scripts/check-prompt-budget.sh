#!/usr/bin/env bash
set -euo pipefail
if (( $# > 1 )); then
  echo "usage: $0 [binary]" >&2
  exit 2
fi

binary=${1:-target/release/kurama}
bundle=$(mktemp)
trap 'rm -f "$bundle"' EXIT

if (( $# == 0 )); then
  cargo build --locked --release -p kurama-cli
fi
if [[ ! -x "$binary" ]]; then
  echo "binary is not executable: $binary" >&2
  exit 2
fi
"$binary" --internal-print-prompt-bundle > "$bundle"
test -s "$bundle"

cargo run \
  --quiet \
  --locked \
  --release \
  --manifest-path tools/prompt-budget/Cargo.toml \
  < "$bundle"
