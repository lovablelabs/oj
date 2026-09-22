#!/usr/bin/env bash
# Regenerate the pinned V8 snapshot bundle for this machine's target.
#
# V8 snapshot creation is not run-deterministic (issue #211), so reproducible
# builds consume a prebuilt per-target bundle via OJ_SNAPSHOT_ARCHIVE (see
# crates/oj_deno_snapshots). This script runs the vanilla snapshot build once
# and harvests its OUT_DIR into snapshot-pins/<target>/.
#
# Run it after any deno_runtime / deno_core / rusty_v8 bump (the consuming
# build fails with a manifest mismatch until you do).
set -euo pipefail
cd "$(dirname "$0")/.."

# A stale pin must not satisfy this build: force the vanilla path.
unset OJ_SNAPSHOT_ARCHIVE
touch crates/oj_deno_snapshots/build.rs
cargo build -p oj_deno_snapshots

out=$(ls -dt target/debug/build/oj_deno_snapshots-*/out | head -1)
target=$(sed -n 's/^target=//p' "$out/OJ_SNAPSHOT_MANIFEST")
pin="snapshot-pins/$target"
rm -rf "$pin"
mkdir -p "$pin"
cp "$out/CLI_SNAPSHOT.bin" "$out/EXTENSION_RESIDUAL_SOURCES.rs" "$out/OJ_SNAPSHOT_MANIFEST" "$pin/"
cp -R "$out/residual_sources" "$pin/residual_sources"
echo "wrote $pin:"
cat "$pin/OJ_SNAPSHOT_MANIFEST"
du -sh "$pin"
