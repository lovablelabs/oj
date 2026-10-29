#!/usr/bin/env bash
# Regenerates tests/golden/get_index.txt with the Go sources (ts-ref at the workspace pin, or
# TSRS_CODEC_COMMIT during an in-progress source upgrade; Go >= 1.27).
set -euo pipefail
crate="$(cd "$(dirname "$0")/.." && pwd)"
ref="$crate/../../ts-ref"
pin="${TSRS_CODEC_COMMIT:-$(sed -n 's/^commit = "\([0-9a-f]\{40\}\)"/\1/p' "$crate/../../Cargo.toml")}"
[ "$(git -C "$ref" rev-parse HEAD)" = "$pin" ] || { echo "ts-ref is not at the pin $pin" >&2; exit 2; }
probe="$ref/tsc/cmd/codecprobe"
trap 'rm -rf "$probe"' EXIT
mkdir -p "$probe"
cp "$crate/gen/goprobe/main.go" "$probe/main.go"
out="$(mktemp -d)"
(cd "$ref/tsc" && GOWORK=off GOTOOLCHAIN=auto go build -o "$out/codecprobe" ./cmd/codecprobe)
"$out/codecprobe" getindex "$crate/tests/fixtures" > "$crate/tests/golden/get_index.txt"
echo "wrote tests/golden/get_index.txt ($(wc -l < "$crate/tests/golden/get_index.txt") lines) from ts-ref $pin"
# Go DecodeNodes + EncodeNode(nil) of every server golden and every client-encoded print input.
mkdir -p "$crate/tests/golden/reencode"
rm -f "$crate/tests/golden/reencode/"*
"$out/codecprobe" reencode "$crate/tests/golden/reencode" "$crate"/tests/golden/*.bin "$crate"/tests/golden/print/*.client.bin
echo "wrote $(ls "$crate/tests/golden/reencode" | wc -l) re-encodings to tests/golden/reencode"
