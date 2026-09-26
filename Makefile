# The vendored Deno forks (oj_deno_*) keep upstream sources verbatim, so they
# are formatted with upstream's rustfmt.toml and not linted as targets.
lint:
	cargo fmt -- --check --color always
	cargo clippy --workspace --all-targets --exclude oj_deno_napi --exclude oj_deno_runtime --exclude oj_deno_snapshots -- -D warnings
