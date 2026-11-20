# The vendored forks (oj_deno_*, oj_notify, oj_rolldown_dev, oj_tsrs_*) keep
# upstream sources verbatim, so they are not linted as targets (oj_deno_*,
# oj_rolldown_dev and oj_tsrs_* are formatted with upstream's rustfmt.toml);
# the forks, reference/ and the e2e fixtures are left out of oxfmt too (see
# .oxfmtrc.json).
JS_FILES = '**/*.{js,mjs,cjs,ts,mts,cts,jsx,tsx}'
# The pinned oxfmt release binary (version and checksums in the script),
# fetched into .tools/ on first use; no Node or npm needed. Run inside the
# recipe so a failed download or checksum fails the target.
OXFMT = oxfmt="$$(tools/fetch-oxfmt.sh)" && "$$oxfmt"
# The pinned cargo-nextest release binary, fetched the same way.
NEXTEST = nextest="$$(tools/fetch-nextest.sh)" && "$$nextest" nextest

lint:
	cargo fmt -- --check --color always
	cargo clippy --workspace --all-targets --exclude oj_deno_napi --exclude oj_notify --exclude oj_rolldown_dev --exclude oj_deno_process --exclude oj_deno_runtime --exclude oj_deno_snapshots --exclude 'oj_tsrs_*' -- -D warnings
	$(OXFMT) --check $(JS_FILES)

fmt:
	cargo fmt
	$(OXFMT) $(JS_FILES)

# The whole Rust suite: nextest runs the test binaries side by side (see
# .config/nextest.toml), which `cargo test` runs one after another; it does
# not run doctests, so those keep `cargo test`.
test:
	$(NEXTEST) run --workspace --no-fail-fast
	cargo test --workspace --doc

.PHONY: lint fmt test
