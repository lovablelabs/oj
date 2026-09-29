# The vendored Deno forks (oj_deno_*) keep upstream sources verbatim, so they
# are formatted with upstream's rustfmt.toml and not linted as targets; the
# same forks, reference/ and the e2e fixtures are left out of oxfmt too (see
# .oxfmtrc.json).
JS_FILES = '**/*.{js,mjs,cjs,ts,mts,cts,jsx,tsx}'
OXFMT = node_modules/.bin/oxfmt

lint: $(OXFMT)
	cargo fmt -- --check --color always
	cargo clippy --workspace --all-targets --exclude oj_deno_napi --exclude oj_deno_runtime --exclude oj_deno_snapshots -- -D warnings
	$(OXFMT) --check $(JS_FILES)

fmt: $(OXFMT)
	cargo fmt
	$(OXFMT) $(JS_FILES)

# oxfmt is pinned in the root package.json; install it on first use.
$(OXFMT): package.json package-lock.json
	npm ci --no-audit --no-fund
	@touch $@

.PHONY: lint fmt
