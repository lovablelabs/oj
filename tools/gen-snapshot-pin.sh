#!/usr/bin/env bash
# Regenerate the pinned V8 snapshot bundle for this machine's target.
#
# V8 snapshot creation is not run-deterministic (issue #211), so reproducible
# builds consume a prebuilt per-target bundle via OJ_SNAPSHOT_ARCHIVE (see
# crates/oj_deno_snapshots). This script runs the vanilla snapshot build once,
# harvests its OUT_DIR into a tarball, and prints the flake.nix file+hash pin
# for it. The bundle lives as an asset on the rolling `snapshot-pins` GitHub
# release, not in the git tree (~8MB of generated artifacts per target, per
# bump); pass --upload to push it there with gh. Asset names embed the
# deno_core version and a blob-hash prefix, so they are immutable: a new
# harvest never overwrites an old asset, and old flake revisions keep
# building.
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
deno_core=$(sed -n 's/^deno_core=//p' "$out/OJ_SNAPSHOT_MANIFEST")
blob8=$(shasum -a 256 "$out/CLI_SNAPSHOT.bin" | cut -c1-8)
name="oj-snapshot-pin-$target-dc$deno_core-$blob8.tar.gz"

tar -czf "$name" -C "$out" \
  CLI_SNAPSHOT.bin EXTENSION_RESIDUAL_SOURCES.rs OJ_SNAPSHOT_MANIFEST residual_sources
if command -v nix >/dev/null; then
  sri=$(nix hash file --sri --type sha256 "$name" 2>/dev/null || nix hash path "$name")
else
  sri="sha256-$(openssl dgst -sha256 -binary "$name" | base64)"
fi

echo "wrote $name ($(du -h "$name" | cut -f1 | tr -d ' ')):"
cat "$out/OJ_SNAPSHOT_MANIFEST"
if [ "${1:-}" = "--upload" ]; then
  gh release upload snapshot-pins "$name"
  echo "uploaded to the snapshot-pins release"
else
  echo
  echo "upload:  gh release upload snapshot-pins $name"
fi
echo
echo "pin in flake.nix snapshotPins:"
echo "  file = \"$name\";"
echo "  hash = \"$sri\";"
