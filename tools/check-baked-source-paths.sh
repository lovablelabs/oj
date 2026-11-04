#!/usr/bin/env bash
# Fail when a library oj builds embeds the absolute checkout path, as a
# non-test `env!("CARGO_MANIFEST_DIR")` or `env!("OUT_DIR")` does.
#
# Bazel's rules_rs (rules_typescript builds oj with it) compiles each crate in
# a sandbox and rejects any rlib that embeds that sandbox's path, so a crate
# that bakes in its source directory builds with cargo but not with Bazel.
# This builds the same way: dev profile (LTO bitcode would hide the string) with
# --remap-path-prefix, so only paths the code itself bakes in remain.
set -euo pipefail

cd "$(dirname "$0")/.."
root=$PWD
target=${CARGO_TARGET_DIR:-target}/baked-source-paths

# Each entry: the crate's library name, then why it may keep the path.
allowed=(
  # deno_core's extension! records each JS file by absolute path; rules_typescript
  # patches the macro to embed the file (deno-core-snapshot-source.patch).
  oj_deno_process
  oj_deno_runtime
  # print_linker_flags names the canonicalized .def file its build script wrote;
  # rules_typescript embeds the file instead (napi-symbol-bytes.patch).
  oj_deno_napi
  # generated_files() lists its OUT_DIR files by absolute path; rules_typescript
  # strips the prefix (cranelift-assembler-isle.patch).
  cranelift_assembler_x64
  # quickjs.c and friends compile from OUT_DIR by absolute path, so __FILE__
  # bakes it in; rules_typescript builds it unpatched.
  rquickjs_sys
)

rlibs=$(
  CARGO_PROFILE_DEV_DEBUG=0 RUSTFLAGS="--remap-path-prefix=$root=." cargo build --locked --workspace --lib --target-dir "$target" \
    --message-format=json-render-diagnostics |
    jq -r 'select(.reason == "compiler-artifact") | .filenames[] | select(endswith(".rlib"))'
)

status=0
baking=()
while read -r rlib; do
  grep -q -a -F "$root" "$rlib" || continue
  name=$(basename "$rlib" .rlib)
  name=${name#lib}
  name=${name%-*}
  baking+=("$name")
  if [[ " ${allowed[*]} " == *" $name "* ]]; then
    continue
  fi
  echo "error: $name embeds the absolute checkout path:" >&2
  strings -a "$rlib" | grep -F "$root" | sort -u | sed 's/^/  /' >&2
  status=1
done <<<"$rlibs"

for name in "${allowed[@]}"; do
  if [[ " ${baking[*]} " != *" $name "* ]]; then
    echo "error: $name no longer embeds the checkout path; drop it from allowed" >&2
    status=1
  fi
done
exit $status
