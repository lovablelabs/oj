#!/bin/sh
# Fetch the pinned cargo-nextest release binary for this host into .tools/,
# verified against its sha256, and print its path. Used by `make test` and CI.
#
# To bump: set NEXTEST_VERSION and replace each sha256 below with the asset
# digests of the new release, e.g.
#   gh api repos/nextest-rs/nextest/releases/tags/cargo-nextest-<version> \
#     --jq '.assets[] | "\(.name) \(.digest)"'
set -eu

NEXTEST_VERSION=0.9.152

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64 | Darwin-x86_64) target=universal-apple-darwin sha=2fadf57df4243e691f088ee3bff4f4ab9af16da0c95a1db84b5cb1685e098a76 ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu sha=ca1f81a5b86533eb45ab97e0064aa88e925e53aab9a698ca6331a7cad1f85f07 ;;
  Linux-x86_64) target=x86_64-unknown-linux-gnu sha=d039b43c3e0958441b6ad0a39c8884d018b3d2e8fb9928f209074b1100b5e17a ;;
  *)
    echo "fetch-nextest: no pinned cargo-nextest build for $(uname -s)-$(uname -m)" >&2
    exit 1
    ;;
esac

root="$(cd "$(dirname "$0")/.." && pwd)"
dir="$root/.tools/nextest-$NEXTEST_VERSION"
bin="$dir/cargo-nextest"
if [ -x "$bin" ]; then
  echo "$bin"
  exit 0
fi

asset="cargo-nextest-$NEXTEST_VERSION-$target.tar.gz"
url="https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-$NEXTEST_VERSION/$asset"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
echo "fetch-nextest: downloading cargo-nextest $NEXTEST_VERSION ($target)" >&2
curl -fsSL --retry 3 -o "$tmp/$asset" "$url"

if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$asset" | cut -d' ' -f1)"
else
  actual="$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)"
fi
if [ "$actual" != "$sha" ]; then
  echo "fetch-nextest: sha256 mismatch for $asset: expected $sha, got $actual" >&2
  exit 1
fi

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$dir"
mv "$tmp/cargo-nextest" "$bin"
chmod +x "$bin"
echo "$bin"
