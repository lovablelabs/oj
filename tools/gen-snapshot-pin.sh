#!/usr/bin/env bash
# Regenerate the pinned V8 snapshot bundle for this machine's target.
#
# V8 snapshot creation is not run-deterministic (issue #211), so reproducible
# builds consume a prebuilt per-target bundle via OJ_SNAPSHOT_ARCHIVE (see
# crates/oj_deno_snapshots). This script runs the vanilla snapshot build once
# and harvests its OUT_DIR into snapshot-pins/<target>/.
#
# The build is `--release -p oj` on purpose: the pin must come from the same
# profile and feature unification the consuming build resolves (a lone
# `-p oj_deno_snapshots` debug build sees a different deno_core feature set),
# and the OUT_DIR is taken from cargo's own build-script-executed message, so
# a custom target dir or stale sibling hash dirs cannot be harvested. Expect
# a full oj release build (~10 min cold).
#
# Run it after any deno_runtime / deno_core / rusty_v8 bump (the consuming
# build fails with a manifest mismatch until you do).
set -euo pipefail
cd "$(dirname "$0")/.."

# A stale pin must not satisfy this build: force the vanilla path.
unset OJ_SNAPSHOT_ARCHIVE
touch crates/oj_deno_snapshots/build.rs
out=$(cargo build --release -p oj --message-format=json \
  | grep '"reason":"build-script-executed"' \
  | grep 'oj_deno_snapshots' \
  | sed -n 's/.*"out_dir":"\([^"]*\)".*/\1/p' | tail -1)
[ -n "$out" ] && [ -f "$out/OJ_SNAPSHOT_MANIFEST" ] || {
  echo "error: no oj_deno_snapshots OUT_DIR with a manifest in the build output" >&2
  exit 1
}

target=$(sed -n 's/^target=//p' "$out/OJ_SNAPSHOT_MANIFEST")
pin="snapshot-pins/$target"
rm -rf "$pin"
mkdir -p "$pin"
cp "$out/CLI_SNAPSHOT.bin" "$out/EXTENSION_RESIDUAL_SOURCES.rs" "$out/OJ_SNAPSHOT_MANIFEST" "$pin/"
cp -R "$out/residual_sources" "$pin/residual_sources"
echo "wrote $pin:"
cat "$pin/OJ_SNAPSHOT_MANIFEST"
du -sh "$pin"
