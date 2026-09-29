#!/bin/sh
# Fetch the pinned oxfmt release binary for this host into .tools/, verified
# against its sha256, and print its path. Used by `make lint` / `make fmt`.
#
# To bump: set OXFMT_VERSION and replace each sha256 below with the asset
# digests of the new release, e.g.
#   gh api repos/oxc-project/oxc/releases/tags/oxfmt_v<version> \
#     --jq '.assets[] | "\(.name) \(.digest)"'
set -eu

OXFMT_VERSION=0.71.0

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target=aarch64-apple-darwin sha=83338a3c3c4bebcfd081712c3f29fd8ae8be2f8ff04b52ed6e4e665c9a5dcd1e ;;
  Darwin-x86_64) target=x86_64-apple-darwin sha=a74bd972803d894127aea9eee6ee09cab5313b8c7850c843956116bdf11a8393 ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu sha=ae72caa2238fb4e91f745131c7fe79f51e0b5490d87490b4bf0177fd61360004 ;;
  Linux-x86_64) target=x86_64-unknown-linux-gnu sha=11707dc99dc78f8e4df9b3d940c214115ec16f62fe259996b2bd75aede04159a ;;
  *)
    echo "fetch-oxfmt: no pinned oxfmt build for $(uname -s)-$(uname -m)" >&2
    exit 1
    ;;
esac

root="$(cd "$(dirname "$0")/.." && pwd)"
dir="$root/.tools/oxfmt-$OXFMT_VERSION"
bin="$dir/oxfmt"
if [ -x "$bin" ]; then
  echo "$bin"
  exit 0
fi

asset="oxfmt-$target.tar.gz"
url="https://github.com/oxc-project/oxc/releases/download/oxfmt_v$OXFMT_VERSION/$asset"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
echo "fetch-oxfmt: downloading oxfmt $OXFMT_VERSION ($target)" >&2
curl -fsSL --retry 3 -o "$tmp/$asset" "$url"

if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$asset" | cut -d' ' -f1)"
else
  actual="$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)"
fi
if [ "$actual" != "$sha" ]; then
  echo "fetch-oxfmt: sha256 mismatch for $asset: expected $sha, got $actual" >&2
  exit 1
fi

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$dir"
mv "$tmp/oxfmt-$target" "$bin"
chmod +x "$bin"
echo "$bin"
