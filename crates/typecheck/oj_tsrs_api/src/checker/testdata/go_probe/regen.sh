#!/usr/bin/env bash
# Regenerates go_probe_b85298b6.jsonl from the pinned Go API session.
#
# Requirements: a clean microsoft/TypeScript checkout at the commit in the workspace Cargo.toml
# ([workspace.metadata.typescript] commit), or TSRS_API_COMMIT during an in-progress source upgrade,
# in ./ts-ref (or $TS_REF), and a Go toolchain matching its
# go.work (go1.27.x). The probe source is copied into the reference checkout only for the duration of
# the run and is removed again (also on failure); the script fails if the checkout is not clean before
# or after.
#
#   git clone --filter=blob:none https://github.com/microsoft/TypeScript.git ts-ref
#   git -C ts-ref checkout b85298b6a81f772d080b0455de0ca9d744cd6fd6
#   crates/tsrs_api/src/checker/testdata/go_probe/regen.sh
#   cargo test -p tsrs_api --test integration checker_test::pinned_go_differential
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(git -C "$here" rev-parse --show-toplevel)
ref=${TS_REF:-$repo/ts-ref}
pinned=${TSRS_API_COMMIT:-$(sed -n 's/^commit = "\([0-9a-f]*\)"$/\1/p' "$repo/Cargo.toml" | head -n1)}
actual=$(git -C "$ref" rev-parse HEAD)
if [ -z "$pinned" ] || [ "$actual" != "$pinned" ]; then
  echo "ts-ref is at $actual, expected pinned $pinned" >&2
  exit 1
fi
if [ -n "$(git -C "$ref" status --porcelain)" ]; then
  echo "ts-ref has local modifications; refusing to run" >&2
  exit 1
fi

dest="$ref/tsc/internal/api/zz_tsrs_probe_test.go"
out="$here/go_probe_${pinned:0:8}.jsonl"
tmp="$out.tmp"
shapes="$here/go_shapes_${pinned:0:8}.jsonl"
shapes_tmp="$shapes.tmp"
flags="$here/go_flags_${pinned:0:8}.jsonl"
flags_tmp="$flags.tmp"
cleanup() { rm -f "$dest" "$tmp" "$shapes_tmp" "$flags_tmp"; }
trap cleanup EXIT
cp "$here/zz_tsrs_probe_test.go" "$dest"

(cd "$ref/tsc" && TSRS_FX="$here/fixture" TSRS_NEEDLES_FILE="$here/needles.txt" TSRS_OUT="$tmp" TSRS_SHAPES_OUT="$shapes_tmp" \
  TSRS_FLAGS_DIR="$here/flags" TSRS_FLAGS_OUT="$flags_tmp" \
  go test ./internal/api -run '^TestTsrsChecker(Probe|Shapes|Flags)$' -count=1)
test -s "$tmp" && test -s "$shapes_tmp" && test -s "$flags_tmp"
mv "$tmp" "$out"
mv "$shapes_tmp" "$shapes"
mv "$flags_tmp" "$flags"
rm -f "$dest"
if [ -n "$(git -C "$ref" status --porcelain)" ]; then
  echo "ts-ref is not clean after the run" >&2
  exit 1
fi
echo "wrote $out ($(wc -l < "$out") lines), $shapes ($(wc -l < "$shapes") lines), $flags ($(wc -l < "$flags") lines) from $(go version) at $pinned"
