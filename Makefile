# reference/ and the e2e fixtures are left out of oxfmt (see .oxfmtrc.json).
JS_FILES = '**/*.{js,mjs,cjs,ts,mts,cts,jsx,tsx}'
# The pinned oxfmt release binary (version and checksums in the script),
# fetched into .tools/ on first use; no Node or npm needed. Run inside the
# recipe so a failed download or checksum fails the target.
OXFMT = oxfmt="$$(tools/fetch-oxfmt.sh)" && "$$oxfmt"

lint:
	cargo fmt -- --check --color always
	cargo clippy --workspace --all-targets -- -D warnings
	$(OXFMT) --check $(JS_FILES)

fmt:
	cargo fmt
	$(OXFMT) $(JS_FILES)

.PHONY: lint fmt
