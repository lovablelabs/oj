# The vendored forks (oj_deno_*, oj_notify, oj_tsrs_*) keep upstream sources
# verbatim, so they are not linted as targets (oj_deno_* and oj_tsrs_* are
# formatted with upstream's rustfmt.toml); the forks, reference/ and the e2e
# fixtures are left out of oxfmt too (see .oxfmtrc.json).
JS_FILES = '**/*.{js,mjs,cjs,ts,mts,cts,jsx,tsx}'
# The pinned oxfmt release binary (version and checksums in the script),
# fetched into .tools/ on first use; no Node or npm needed. Run inside the
# recipe so a failed download or checksum fails the target.
OXFMT = oxfmt="$$(tools/fetch-oxfmt.sh)" && "$$oxfmt"

lint:
	cargo fmt -- --check --color always
	cargo clippy --workspace --all-targets --exclude oj_deno_napi --exclude oj_notify --exclude oj_deno_process --exclude oj_deno_runtime --exclude oj_deno_snapshots --exclude 'oj_tsrs_*' -- -D warnings
	$(OXFMT) --check $(JS_FILES)

fmt:
	cargo fmt
	$(OXFMT) $(JS_FILES)

.PHONY: lint fmt
