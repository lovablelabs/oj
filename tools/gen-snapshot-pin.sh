#!/usr/bin/env bash
# Regenerate the pinned V8 snapshot bundle for this machine's target.
#
# V8 snapshot creation is not run-deterministic (issue #211), so reproducible
# builds consume a prebuilt per-target bundle via OJ_SNAPSHOT_ARCHIVE (see
# crates/oj_deno_snapshots). This script runs the vanilla snapshot build once,
# harvests its OUT_DIR into a tarball, and prints the flake.nix pin for it.
# The bundle lives as an asset on a version release, not in the git tree
# (~8MB of generated artifacts per target, per bump); the release workflow
# runs this automatically when a deno bump made the pin stale, and
# `--upload <release-tag>` is the manual fallback (e.g. after a rusty_v8-only
# bump the staleness proxy misses). Asset names embed the deno_core version
# and a blob-hash prefix, so they are immutable: a new harvest never
# overwrites an old asset, and old flake revisions keep building.
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

# COPYFILE_DISABLE: no AppleDouble ._ entries from macOS bsdtar.
COPYFILE_DISABLE=1 tar -czf "$name" -C "$out" \
  CLI_SNAPSHOT.bin EXTENSION_RESIDUAL_SOURCES.rs OJ_SNAPSHOT_MANIFEST residual_sources
if command -v nix >/dev/null; then
  sri=$(nix hash file --sri --type sha256 "$name" 2>/dev/null || nix hash path "$name")
else
  sri="sha256-$(openssl dgst -sha256 -binary "$name" | base64)"
fi

echo "wrote $name ($(du -h "$name" | cut -f1 | tr -d ' ')):"
cat "$out/OJ_SNAPSHOT_MANIFEST"
release="${2:-}"
if [ "${1:-}" = "--upload" ]; then
  [ -n "$release" ] || { echo "usage: $0 --upload <release-tag>" >&2; exit 1; }
  gh release upload "$release" "$name"
  echo "uploaded to the $release release"
else
  echo
  echo "upload:  gh release upload <release-tag> $name"
fi
echo
echo "pin in flake.nix snapshotPins:"
echo "  release = \"${release:-<release-tag>}\";"
echo "  file = \"$name\";"
echo "  hash = \"$sri\";"

# Machine-readable outputs for the snapshot-pin workflow.
if [ -n "${GITHUB_OUTPUT:-}" ]; then
  {
    echo "pin_target=$target"
    echo "pin_file=$name"
    echo "pin_hash=$sri"
  } >> "$GITHUB_OUTPUT"
fi
