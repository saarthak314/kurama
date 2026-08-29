#!/usr/bin/env bash
set -euo pipefail
binary=${1:-target/release/kurama}
limit=$((10 * 1024 * 1024))
test -f "$binary"
case "$(uname -s)" in
  Darwin) strip -x "$binary" ;;
  Linux) strip "$binary" ;;
  *) echo "unsupported release host" >&2; exit 2 ;;
esac
size=$(wc -c < "$binary" | tr -d ' ')
printf 'kurama bytes=%s limit=%s\n' "$size" "$limit"
test "$size" -le "$limit"
