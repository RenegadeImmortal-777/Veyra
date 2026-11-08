#!/usr/bin/env sh
set -eu
ROOT="$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"
if [ -f "$ROOT/engine-rs/target/release/veyra" ]; then
  EXE="$ROOT/engine-rs/target/release/veyra"
else
  EXE="$ROOT/engine-rs/target/debug/veyra"
fi
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
  (
    cd "$project" &&
    "$EXE" add "$name" >/dev/null &&
    printf 'use "%s";\nlet pkg = import_package("%s");\nassert(len(keys(pkg)) > 0);\n' "$name" "$name" > src/main.vy &&
    "$EXE" check >/dev/null &&
    "$EXE" run >/dev/null
  ) || FAILED=$((FAILED+1))
done
echo "TOTAL=$TOTAL"
echo "FAILED=$FAILED"
test "$FAILED" -eq 0
