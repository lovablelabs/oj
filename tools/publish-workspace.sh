#!/usr/bin/env bash
# Publish every workspace crate to crates.io in dependency order (path deps
# first, so the forks land before oj_js, and oj last). Idempotent: a version
# already on crates.io is skipped, so a partially failed release run can just
# be re-run. Needs CARGO_REGISTRY_TOKEN.
set -euo pipefail
cd "$(dirname "$0")/.."

order=$(python3 - <<'EOF'
import json, subprocess
meta = json.loads(subprocess.check_output(
    ["cargo", "metadata", "--format-version", "1"]))
members = set(meta["workspace_members"])
ws = [p for p in meta["packages"] if p["id"] in members]
names = {p["name"] for p in ws}
# publish == [] means `publish = false`.
skip = {p["name"] for p in ws if p.get("publish") == []}
deps = {p["name"]: {d["name"] for d in p["dependencies"]} & names for p in ws}
done, out = set(), []
while len(done) < len(deps):
    ready = sorted(n for n in deps if n not in done and deps[n] <= done)
    assert ready, f"dependency cycle among {sorted(set(deps) - done)}"
    done.update(ready)
    out += [n for n in ready if n not in skip]
print("\n".join(out))
EOF
)

for crate in $order; do
  echo "--- publishing $crate"
  if out=$(cargo publish -p "$crate" --locked 2>&1); then
    echo "$out" | tail -2
  elif echo "$out" | grep -q "already uploaded\|is already uploaded\|already exists"; then
    echo "skip: this version of $crate is already on crates.io"
  else
    echo "$out"
    exit 1
  fi
done
echo "all crates published"
