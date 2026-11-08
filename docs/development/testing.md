# Testing strategy

oj runs other people's source code. Everything it reads -- modules, stylesheets,
`.env` files, `package.json`, `tsconfig.json`, `oj.config.ts`, cache entries on
disk, request paths off the wire -- is input it did not write, and much of it is
machine-generated. So the suite is organized around *how a thing can go wrong*
rather than around modules: adversarial input, boundary shapes, contention,
injected faults, and properties that have to hold for every input.

The rules that shape it:

- **Totality.** Arbitrary bytes are a legal file. Every entry point returns a
  described error or a result -- never a panic, never a hang.
- **Output validity.** Anything the compiler emits parses as JavaScript. Tests
  assert that by re-parsing the output, not by matching strings.
- **Confinement.** An unprefixed environment variable never reaches the client.
  A request path never reaches a file outside the project.
- **Determinism.** Compilation is a pure function of (path, source, options);
  the cache keys entries on exactly that, so a warm cache cannot be wrong.
- **Degradation.** The cache is an optimization: every failure mode costs a
  recompile and nothing else.

## Layers

### Unit tests (in `crates/*/src/`)

Next to the code, for the logic that has no interesting failure surface of its
own: define construction, HMR boundary rules, CJS lowering shapes, the dev
server's private path helpers (percent-decoding, traversal rejection, url
classification, HMR gate relevance).

### Integration suites (in `crates/*/tests/`)

| Suite | Proves |
| --- | --- |
| `oj_compiler/tests/adverse.rs` | Malformed, hostile and enormous source: unterminated everything, deep nesting on a compile-sized stack, unicode and bidi identifiers, lone surrogates, hostile import specifiers, a rewriter that returns junk, JSON that is not JSON, `__proto__` keys, every `require` shape, glob and dynamic-import-vars patterns that try to escape |
| `oj_compiler/tests/edge_cases.rs` | Empty files, BOMs, shebangs, CRLF, TypeScript-only syntax (including `enum`), JSX per extension, top-level await, every export form, dev-vs-prod instrumentation, sourcemap omission for synthesized modules, CJS/ESM factory selection |
| `oj_compiler/tests/properties.rs` | Totality over arbitrary bytes, output validity, determinism, output as a parser fixed point, import order and one-shot rewriting, mode-independence of the module graph |
| `oj_env/tests/adverse.rs` | Malformed `.env` lines, self-referential and mutual expansion, unterminated quotes and braces, non-ASCII values byte for byte, `%`-soup in HTML, secret confinement including near-miss prefixes and values that try to break out of the JSON literal |
| `oj_env/tests/properties.rs` | Parsing is total; single-quoted and metacharacter-free values are byte-exact; the defines and the aggregate `import.meta.env` object always agree; no unprefixed variable is ever exposed |
| `oj_graph/tests/model.rs` | Model-based property test: random link/accept/change sequences checked against a from-scratch reference model after every operation, plus boundary-set and idempotence invariants |
| `oj_graph/tests/adverse.rs` | 200k-deep import chains, 20k-wide fan-in, cycles of every length, diamonds, self-imports, repeated re-linking leaving no stale edges -- all on a runtime-worker-sized stack |
| `oj_cache/tests/faults.rs` | Garbage, truncated, wrong-schema and older-schema entries; read-only directories; a cache dir that is a file; keys that are not digests; version isolation |
| `oj_cache/tests/concurrency.rs` | Several builds sharing one `.oj-cache`: concurrent writers publish a complete entry, a reader never observes a partial one, distinct keys all land, eviction races safely with rewriting |
| `oj_cache/tests/properties.rs` | Keys are hex digests, deterministic, and injective over (source, url, mode, version); entries round trip exactly; non-digest keys are refused |
| `oj_css/tests/adverse.rs` | Malformed and truncated stylesheets, 20k-rule and 1MB-value files, nesting within the supported envelope, module scoping collisions, Sass errors, import confinement, minification as formatting only |
| `oj_resolver/tests/adverse.rs` | Degenerate specifiers, traversal out of the project (allowed by design -- see below), malformed `package.json` and `tsconfig.json`, exports-map encapsulation, private `#imports`, symlinks and symlink cycles, dedupe and alias policy, non-file schemes |
| `oj_config/tests/adverse.rs` | A config that never finishes, allocates without bound, recurses, throws, is cyclic, exports nothing, or is not an object; TypeScript stripping including `enum`; the sandbox's reach; candidate precedence |

### End-to-end (in `e2e/`)

Node scripts driving the real CLI, some with Playwright. `e2e/run.mjs` is the
main suite; the standalone scripts cover one seam each. `e2e/awkward-paths.mjs`
is the request-path pair: filenames that need percent-encoding must be served,
and every spelling of a traversal (raw, `%2e%2e`, `%2f`, `/@fs`) must not reach
a file outside the app, in both `oj dev` and `oj preview`.

### Fuzzing (`fuzz/`)

cargo-fuzz targets over the untrusted-input surfaces, asserting the invariants
above rather than merely "does not crash":

| Target | Asserts |
| --- | --- |
| `fuzz_compile` | Output re-parses, specifiers are well-formed, compilation is deterministic |
| `fuzz_dotenv` | Keys are well-formed; the defines and the aggregate object agree; no unprefixed variable or value leaks |
| `fuzz_json_module` | Valid JSON always yields a valid module with exactly one default export and no bare `__proto__` key |
| `fuzz_html_env` | Substitution is total, only touches known keys, never re-scans its own output |
| `fuzz_cache_entry` | Any entry the cache accepts round trips unchanged |

```sh
cargo install cargo-fuzz                       # needs a nightly toolchain
cargo +nightly fuzz run fuzz_compile -- -max_total_time=300
```

`fuzz_compile` skips inputs nested deeper than 200 brackets: recursion depth is
governed by the stack a compile thread gets, which is a separate contract (see
below), and without the filter the fuzzer spends all its time rediscovering that
parsers recurse.

### Mutation testing (`.cargo/mutants.toml`)

Run on demand, not in CI: it is a check on the suites, not a gate on a change.
Scoped to the pure crates -- the dev server and the CLI are covered from `e2e/`,
which cargo-mutants cannot drive.

```sh
cargo mutants --test-workspace false -j 4 -p adaptive-oj-env -p adaptive-oj-graph -p adaptive-oj-cache -p adaptive-oj-css
```

The survivors that mattered were killed by adding tests: the CSS browser-target
matrix (a matrix decoding to version 0 downlevels everything, so the tests now
assert that supported syntax survives) and the CSS module naming pattern. One
survivor was a dead expression rather than an untested one -- the index computed
for an unclosed `${` in `oj_env::expand` was never read -- and was removed. A
timeout counts as caught: those mutants turn a loop counter into a
multiplication and hang, which the suite catches as a hang.

`oj_env` and `oj_css` now come back 95 caught / 6 timed out / 6 missed of 108,
and all six survivors are equivalent mutants rather than gaps:

- deleting any *single* browser from the target matrix (five of them): the
  remaining entries still drive the same output, since downleveling follows the
  oldest target;
- flipping the `env.is_empty() || !html.contains('%')` fast path in
  `replace_html_env`: without it the loop reaches the same answer, only slower.

New survivors in these crates mean a real gap, not noise.

## Deliberate boundaries

Things that look like gaps and are not, pinned by a test so they stay decisions:

- **Recursion depth is a stack-size contract.** Parsing, transforming and
  printing all recurse once per nesting level, and a stack overflow aborts the
  process rather than failing one file. `oj_compiler::COMPILE_STACK_SIZE` is what
  the CLI installs as the runtime's thread stack size, and the adverse suites
  test at that size. There is no depth at which recursion is safe for every
  stack; there is a documented envelope, far above anything hand-written.
- **Unbounded Sass recursion aborts the process.** `grass` has no call-depth
  limit and Rust cannot catch a stack overflow, so `@mixin a { @include a; }`
  takes the process down. Containing it needs process isolation, not a bigger
  stack. `oj_css/tests/adverse.rs` keeps the case runnable under `--ignored` so
  it can be re-checked after a `grass` upgrade.
- **Early errors are the browser's to report.** Duplicate bindings, assignment
  to a `const`, `with` in a module: oj does not run oxc's semantic checker on
  every file in dev. The emitted code is exactly as wrong as the input, never
  silently repaired.
- **Resolution is not confinement.** A linked package or a monorepo sibling
  lives outside the project root, so resolution deliberately returns absolute
  paths outside it -- including through a `file://` specifier. The dev server's
  `/@fs` allow-list is what decides whether such a path may be served, and
  `e2e/awkward-paths.mjs` tests that half.
- **A broken `tsconfig.json` fails every resolution.** Guessing the alias table
  would resolve imports to the wrong files, so the error is reported instead --
  and every message names the tsconfig.
- **`%%KEY%%` differs from Vite.** Vite's `/%(\S+?)%/g` consumes `%%KEY%` as an
  unknown key and leaves the text alone; oj rescans from the inner `%` and
  substitutes. Only adjacent percents differ.

## Commands

```sh
cargo test --workspace                 # unit + integration, all crates
cargo test -p adaptive-oj-graph --test model    # one suite
cargo test --workspace -- --ignored    # the process-aborting tier, deliberately
cargo check --manifest-path fuzz/Cargo.toml   # fuzz targets still build
node e2e/run.mjs                       # the main end-to-end suite
node e2e/awkward-paths.mjs             # one seam
```

## Conventions

- Name a test after the property it defends, not the function it calls.
- Assert the invariant, not the shape: re-parse emitted code, compare against a
  reference model, check that a secret is absent -- rather than matching output
  text that a harmless refactor will change.
- Adverse input goes in the suite that owns the seam. A new failure mode gets a
  test there, not a new suite.
- Randomness is seeded (proptest). Failures reproduce from the seed alone, and
  proptest writes a regression file next to the suite -- commit it.
- When a test documents a divergence or a limitation rather than a requirement,
  say so in a comment and list it under **Deliberate boundaries** above.
