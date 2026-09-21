#!/usr/bin/env sh
set -eu
ROOT="$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"
EXE="$ROOT/engine-rs/target/debug/veyra"
export VEYRA_HOME="$ROOT"
TMPBASE="${TMPDIR:-/tmp}/veyra-package-matrix"
rm -rf "$TMPBASE"
mkdir -p "$TMPBASE"
if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for package-matrix.sh" >&2
  exit 2
fi
FAILED=0
TOTAL=0
for name in $(jq -r '.packages | keys[]' "$ROOT/registry/index.json"); do
  TOTAL=$((TOTAL+1))
  project="$TMPBASE/$name"
  "$EXE" new "$project" >/dev/null
  (cd "$project" && "$EXE" add "$name" >/dev/null && printf 'use "%s";\nprint("PACKAGE_OK");\n' "$name" > src/main.vy && "$EXE" check >/dev/null) || FAILED=$((FAILED+1))
done
echo "TOTAL=$TOTAL"
echo "FAILED=$FAILED"
test "$FAILED" -eq 0
