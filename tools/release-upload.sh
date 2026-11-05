#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Copyright (c) 2026 Raphael Amorim
#
# release-upload.sh <tag> <file>: attaches <file> to the release <tag>, done
# once the release holds an asset of that name whose digest is the file's.
#
# `gh release upload` retries a request whose answer it lost; when the first
# one had landed, the retry finds its own asset and fails with "ReleaseAsset.
# name already exists" (HTTP 422), `--clobber` or not. The asset is then the
# file already, which the digest GitHub records for it (sha256) shows. An
# asset of that name with other bytes is replaced (`--clobber`), so a rerun
# of the job still attaches its own build.
set -euo pipefail

tag=$1
file=$2
name=$(basename "$file")
want="sha256:$(shasum -a 256 "$file" | cut -d' ' -f1)"

attached() {
  gh api "repos/${GITHUB_REPOSITORY}/releases/tags/${tag}" \
    --jq ".assets[] | select(.name == \"${name}\") | .digest" 2>/dev/null
}

for attempt in 1 2 3; do
  if gh release upload --clobber "$tag" "$file"; then
    exit 0
  fi
  if [ "$(attached)" = "$want" ]; then
    echo "${name} is on ${tag} already (${want})"
    exit 0
  fi
  echo "upload attempt ${attempt} of ${name} failed" >&2
  sleep $((attempt * 10))
done
echo "${name} is not on ${tag} with digest ${want}: $(attached || true)" >&2
exit 1
