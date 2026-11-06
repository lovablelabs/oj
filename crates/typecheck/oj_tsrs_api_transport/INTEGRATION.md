> Historical integration review. The duplicate transport request-filesystem implementation and its `requestfs`
> feature have since been removed. The fixture comparisons below now run against `tsrs_api::requestfs` in
> `crates/tsrs_api/tests/integration/requestfs_differential.rs`; the fixtures and generator live beside that test.

# Runtime integration review for core candidate a9b4354

> Historical record: the reviews of the Node API integration (PR 34) before it merged, kept because code comments
> cite it. The work it plans has landed, including strict params JSON, request-filesystem rebasing and
> `fileNotifications` alias expansion, `ContentionWait` / `Holder` in the checker lease, `ScopedFs` and exact integer
> lexemes. Statements about "core today" describe the reviewed candidate, not main; see docs/NODE_API.md and this
> crate's README.md for current behavior. Artifact paths under `/root/artifacts` are not in the repository.

This review covers the core candidate `a9b435432f4fa38e140fccacc50d461d15e4ce90` (`mfs-cx/node-api-core`), which
contains this crate at fe051ba. The reference is the pinned Go server built at b85298b (`go build ./cmd/tsc`).
Both servers were driven by the pinned Node clients.
Core and the checker lane own every change listed below; this crate does not edit their files.

## Callback re-entry (tests/node/reentry_repro.mjs)

Each scenario runs in its own child process, killed after 20 s. The outer request is `createSnapshot`
of a 50-file project, or `build` for the `*DuringBuild` rows. While the server reads `b.ts` through the
`readFile` callback, the client makes the nested request listed below, in both sync and async modes.

| nested request | pinned Go | tsrs a9b4354 |
| --- | --- | --- |
| ping, parseConfigFile, createSnapshot (other project) | ok | ok |
| getSemanticDiagnostics on an earlier snapshot | ok | ok |
| updateSnapshot / release of an earlier snapshot | ok | ok |
| createSnapshot of the same project, or of the in-flight project / file | ok | ok |
| ping during an orchestrator build | ok | ok |
| build on the orchestrator that is building | **hang** (killed) | deliberate `api: client error: the build orchestrator is in use…` |

Result: 22 of 22 scenarios finish under tsrs. In tsrs_project, no snapshot operation hangs when it
re-enters, including parsing the in-flight project from several worker threads.

Core today guards one lock (`build.rs` build orchestrators) with `lock_for_request`.
The other session locks are all short-lived and never held across filesystem access (`snapshots`,
`batch_pages`, `source_files.*`, `module_resolvers.*`). Keep it that way. Any lock or exclusive lease that is
held while the program or config is read through the session filesystem must use
`tsrs_api_transport::lock_for_request`, or check `blocking_may_deadlock()` and return
`reentrancy::reentrancy_error(..)`. Examples: a future languageServerUpdateMu equivalent, a module-resolver
context held during resolution, or the checker lease (owned by the checker lane).

## Close and cancel (tests/node/close_repro.mjs)

The client disconnects while the server waits on a `readFile` callback it will never get an answer to.

| case | pinned Go | tsrs a9b4354 |
| --- | --- | --- |
| stdin EOF (sync, async) | exit 0, under 2 ms | exit 0, under 2 ms. stderr carried a panic report; fixed in this crate (see below) |
| client vanishes (stdin and stdout closed) | killed by SIGPIPE | exit 1, `ipc: failed to write response: Broken pipe` |

The fix: a callback failure now unwinds with `resume_unwind`, so no panic report reaches stderr, which
matches Go's silent recover. I re-ran the repro against a9b4354 with only `src/callbackfs.rs` replaced;
stderr is empty for EOF. Genuine handler panics are still reported.

`SessionHandler` (in `crates/tsrs_cli/src/api.rs`) does not read `RequestContext.cancel`. After EOF, a
long request that makes no callbacks runs to completion before `run` returns and joins it. Go behaves the
same unless its project code observes ctx.

## Strict JSON for sync request params (core: `session.rs` `Session::dispatch`, which now calls `strictjson::decode`)

Async requests are validated by the transport (`jsonrpc::decode_message`). In Go, a violation there is
fatal to the connection, and tsrs matches. Sync (MessagePack) params reach core as raw bytes and are parsed
with `tsrs_core::json`. That parser rejects duplicate keys but accepts unpaired surrogate escapes. Probes
against both binaries:

| sync request | pinned Go | tsrs a9b4354 |
| --- | --- | --- |
| `createSnapshot` with file content `"x\ud800y"` | `api: invalid request: failed to unmarshal *api.CreateSnapshotParams: jsontext: invalid surrogate pair …` | accepted |
| duplicate `/p/a.ts` in `fileSystem.files` | `… jsontext: duplicate object member name "/p/a.ts" within "/fileSystem/files"` | rejected, text `… at offset 65` |
| `parseConfigFile` with `"/p/\udc00.json"` | invalid request (surrogate) | `api: client error: could not read file "/p/\u{fffd}.json"` |
| `echo "x\ud800"` | echoes the raw bytes | same |

Plan for core: in `parse_params`, for every method except `echo` and `ping`, call
`tsrs_api_transport::strictjson::validate(params)` before `json::unmarshal`. On error, return
`ApiError::invalid_request(format!("failed to unmarshal *api.{GoParamsType}: {e}"))`.
Go's text is `api: invalid request: failed to unmarshal *api.<Type>Params: <jsontext error>`, and
`strictjson` already produces that jsontext error text exactly (tests/strictjson_oracle.rs).

## Request filesystem

(Resolved after this review: core's `requestfs.rs` now implements `LayeredFileSystem`, `RebasableFileSystem` and
`FileChangeExpander`, and tsrs_project rebases it.)

Core keeps its own `crates/tsrs_api/src/requestfs.rs` instead of this crate's `requestfs` feature.
Its filesystem behaviour matches pinned Go on this crate's differential fixture: I ran core's
`RequestParams::parse` + `new_for_update` over `tests/fixtures/requestfs_scenarios.json` and
`requestfs_go.json` with a scratch test that I did not commit. All 242 comparisons (FS queries plus per-step file-change summaries) agree. Structural
differences from Go that the fixture does not reach:

- The base is the host FS (`Arc<dyn FS>`), not the snapshot's layered or overlay FS. There is no
  `overlays()`, no `FileHandleSource`, no `FileChangeExpander`, and no rebasing in tsrs_project's
  `layer_overlay_file_system`. Go uses these for overlays under request layers (language-server
  snapshots) and to expand `fileNotifications` to symlink aliases. I could not demonstrate a visible
  difference: an alias-invalidation probe showed no diagnostics change in either Go or tsrs, so it was
  inconclusive.
- `add_file_changes` ignores the "overlay dropped by the new layer" branch (`_fs` is unused).

Plan: either switch core to `tsrs_api_transport::requestfs` (`AnyFs`, `RequestFs`; it implements
`LayeredFileSystem`, `FileChangeExpander` and `base_file_system`/`with_base_file_system`), or port
those trait impls into core's type. Then add the rebasable hook in `tsrs_project::layer_overlay_file_system`
(Go `overlayfs.go:226`).

## Not supported (explicit)

Windows named pipes (`--pipe` on non-Unix returns Unsupported) and external content mappers.

# Review of core d6d8e2a (PR 34)

**Inputs**
- Core checkout: `d6d8e2a540cf18feb5d03286ae069968a701bee4`, in an isolated worktree.
  - `target/debug/tsrs` sha256 `9d65e458…6304`. A clean rebuild of the same head produced `5e114214…5883`; debug builds are not byte-reproducible.
- Pinned Go: `go build ./cmd/tsc` at microsoft/TypeScript `b85298b6a81f772d080b0455de0ca9d744cd6fd6`, sha256 `98159140…eacc`.
- Clients:
  - pinned `packages/typescript/src/api` at the same commit;
  - the SDK at `npm/tsrs/src/api` as of 1387320 (from d6).
- Harness: the scripts in `tests/node/`.

## Strict request JSON: matches, with a gap in type errors

Probe: all 172 pinned methods, sync, with malformed params, against both binaries. Per-method results are
in the `d6-strict-probe-{go,tsrs}.json` artifacts.

| params | result |
| --- | --- |
| lone surrogate (`{"snapshot":"\ud800"}`) | 171/172 identical text |
| top-level duplicate | 171/172 identical |
| nested duplicate (`{"x":{"y":[{"z":1,"z":2}]}}`) | 171/172 identical |
| invalid UTF-8 | 171/172 identical |
| truncated JSON | 171/172 identical |

The one difference is `stopCPUProfile`, which is explicitly unsupported.

Remaining gaps (core: `session.rs` `parse_params` / `wire.rs`):

- **Wrong top-level type** (`[1]`): Go rejects all 171 methods that take params with
  `api: invalid request: failed to unmarshal *api.<T>: json: cannot unmarshal JSON array into Go api.<T>`.
  tsrs:
  - 121 methods answer `invalid request`, but with other text;
  - 37 answer `client error`;
  - 5 succeed (`createBuildOrchestrator` creates an orchestrator; also `parseCommandLine`,
    `createSourceFile`, `transpileModule`, `transpileDeclaration`);
  - 5 return untyped `build orchestrator not found` errors.

  Fix: after `strictjson::validate`, reject any non-object, non-null params for methods that
  take params, using Go's text.
- **Wrong field types** (`{"snapshot":"x","project":7,...}`):
  - 148 methods are `invalid request` in both, but the text differs. Go says
    `json: cannot unmarshal JSON string into Go api.SnapshotID within "/snapshot"`; tsrs says
    `snapshot must be a non-negative integer`.
  - 3 methods differ in class (Go `invalid request`, tsrs `client error`): `retainSourceFile`
    and two others.

## Callback re-entry and EOF on d6: unchanged and bounded

- `reentry_repro.mjs`: all 22 scenarios DONE; nested snapshot operations succeed as in Go.
- `buildDuringBuild` returns the deliberate error, where Go hangs.
- `close_repro.mjs`:
  - EOF while a callback is pending: exit 0 within 2 ms, stderr empty;
  - client gone: exit 1, `Broken pipe`.

## Async false positives in the re-entrancy gates: found and fixed in the transport

`tests/node/contention_repro.mjs` sets up this situation: request C waits on a slow client callback
without holding anything, while requests A and B contend for the same orchestrator or API checker.
Nothing re-enters, and Go serves everything.

| resource, 3 rounds | pinned Go | d6 | d6 + this fix |
| --- | --- | --- | --- |
| build orchestrator | 0 errors | 6 spurious `…is in use by a request that is waiting on a client callback` | 0 |
| API checker (checker lane's `lease.rs`) | 0 | 2 spurious | 0 |

Cause: `blocking_may_deadlock()` treated any request that was waiting on the client, anywhere on the
connection, as proof of re-entry.

Fix: `reentrancy.rs`; no change to the signature or call sites.
- **Sync** is unchanged. A conflict there is provably a nested re-entry, so it still fails immediately.
- **Async** waits out a grace period before failing: `ConnOptions` / `ServeOptions::reentrancy_grace`,
  default 10 s. A real async re-entry deadlock now ends with the same error after the grace period
  instead of failing early on valid concurrency.

Verification:
- I applied the change to a local copy of d6 and rebuilt; I did not commit it to core.
- `contention_repro.mjs`: 0 errors for both resources.
- `reentry_repro.mjs`: async `buildDuringBuild` errors after 10.3 s; the other re-entry scenarios are unchanged.
- The checker lane's `session_tests::lease_reentrancy` tests: 3/3 pass, in 10 s instead of instantly. They
  can set a short `reentrancy_grace` in their `ConnOptions` if they want the old speed.
- New transport test: `async_unrelated_slow_callback_does_not_reject_ordinary_contention`.

## Checker lease gate (270978d)

`lease.rs` takes a per-program gate before the API checker slot and uses `blocking_may_deadlock()`.
- Lock order, gates map before gate, is consistent between acquire, drop and forget. No inversion.
- Entries are removed when unused.
- The address-keyed map is only used while the program is alive.

Its async false positive came from the transport predicate and is fixed above. No checker-file change
is required.

## SDK connection-loss patch (1387320)

`tests/node/connloss_repro.mjs` checks, for each case, that every promise settles and that there is no
`uncaughtException` or `unhandledRejection`. Each case runs in its own child process, killed after 20 s.

Results with the SDK at d6, against both tsrs d6 and Go:
- All 5 cases settle (kill during callback, close after death, kill-then-request, close during callback,
  kill during batch).
- No uncaught exceptions or unhandled rejections.
- `close()` resolves.

The unpatched pinned client against tsrs d6 leaves three cases unsettled (process exit 13): kill during
callback, kill-then-request, and kill during batch. So the patch is needed.

Cosmetic differences:
- `close()` during a callback rejects the in-flight request with vscode-jsonrpc's
  `Pending response rejected since connection got disposed` rather than the patch's error. This is
  deliberate: it was the client's own close.
- The detail text depends on which signal arrives first (EPIPE vs closed).

## Request filesystem alias gap: concrete repro and bounded fix plan (core)

`tests/node/requestfs_alias_repro.mjs`:
- A request layer adds symlink `p/link -> p/target` (a host directory). The project compiles `link/a.ts`.
- The host file `p/target/a.ts` is fixed on disk, and `updateSnapshot` notifies the change at the
  target path only.

| semantic diagnostics before → after, by notification | target path | alias path | none |
| --- | --- | --- | --- |
| pinned Go | 1 → 0 | 1 → 0 | 1 → 1 |
| tsrs d6 | **1 → 1 (stale)** | 1 → 0 | 1 → 1 |

Go expands notifications to request-symlink aliases in `snapshot.processFileChanges`
(`FileChangeExpander`). Core passes its request filesystem to tsrs_project as a plain `FS`, so the
expansion never runs.

Fix, about 18 lines (artifact `d6-core-alias-expansion-proposal.patch`):
1. Add `pub fn expand_file_changes(&self, FileChangeSummary) -> FileChangeSummary` to core's
   `RequestFileSystem`. It is a port of `filechanges.go` `ExpandFileChanges` and uses the existing
   `aliases_for_path`.
2. In `snapshots.rs`, apply it to `file_changes` with the new snapshot's request FS just before
   `clone_snapshot`. The patch covers update; do the same in `handle_create_snapshot`.

With this applied locally, tsrs gives `target: 1 → 0`, matching Go.

Overlay rebasing and the "overlay dropped" branch are only reachable from LSP-connected API sessions.
`getCurrentLanguageServerSnapshot` returns `requires an LSP-connected API session` in both Go and tsrs
for `--api`, so standalone `--api` cannot observe them. That remains a documented gap.

# Follow-up on 623ee28: callback ownership, and grace measured per acquisition

## Can callback ownership reach the resource holder?

- **The protocol does not carry it.** Sync callbacks are matched by method name, and async ones by
  `api<N>` IDs that only the server side knows. A callback can only be attributed to the request whose
  thread made the call.
- **Most callbacks come from worker threads.** I instrumented d6 locally (not committed) and ran a
  40-file `createSnapshot` + `build`. 260 of the 262 filesystem callbacks were made from compiler worker
  threads that serve no request. Only 2 came from request threads.
- **The transport now tracks what it can:**
  - every request has a `RequestState`;
  - calls made on a request thread are attributed to that request;
  - `lock_for_request` records the lock's holder;
  - `ContentionWait` accepts a `Holder`;
  - a waiter whose holder is waiting on a request it can see follows that chain; a wait-for cycle
    counts as stuck.
- **What that means for a waiter:**
  - it is rejected only when the holder (or the chain behind it) has an attributed call in flight, or
    when some unattributed call is in flight;
  - an unrelated attributed callback no longer counts against it;
  - otherwise it waits like Go, however long the holder takes.
- **What remains impossible without core help:** attributing worker-thread file reads. Core could
  propagate request identity into the filesystem that a request's program build uses, so worker reads
  carry it. That is not done.
- **Bounded divergence, async only, not claimed fixed:** a waiter that has nothing to do with the
  re-entry is rejected after the grace period (default 10 s) in two cases. Pinned Go waits forever in
  both.
  1. An unattributed callback (a worker-thread read on the connection) stays pending longer than the
     grace period.
  2. The holder itself waits on its own client callback for longer than the grace period.
- **Genuine re-entry is unchanged:** sync fails immediately, async fails after the grace period, and
  the cancel/EOF exits stay in place.

## Per-acquisition grace

- **Problem:** `blocking_may_deadlock()` keeps the grace start in a thread-local, reset only after a
  200 ms gap. The checker lease never resets it on success. So a second contended acquisition by the
  same request within 200 ms inherits the first wait's start. Regression test: with a 100 ms grace, the
  legacy path rejects the second wait about 20 ms in.
- **Fix:** `ContentionWait` keeps the state per acquisition. `lock_for_request` uses it.
- **Legacy predicate:** `blocking_may_deadlock()` is kept for compatibility, now keyed by request. It
  still has the same-request inheritance problem, which is documented in the code and covered by a test.
- **Checker lane (owners):** migrate `lease.rs` to the per-acquisition API, about 3 lines, no signature
  changes elsewhere.
  - When acquiring the gate: `gate.holder = Holder::current()`.
  - In the wait loop: `let mut wait = ContentionWait::new(gate.holder.as_ref());`, then
    `wait.set_holder(...)` and `if wait.may_deadlock() { … }` in place of `blocking_may_deadlock()`.

## Tests

`tests/reentrancy.rs` gains these real-transport tests, using a 100 ms grace and handler holds of at most 1 s:

| test | result |
| --- | --- |
| unrelated attributed callback and holder both outlast the grace period | the waiter succeeds, as in Go; with 623ee28 it would have been rejected |
| unattributed callback outlasts the grace period | the waiter is rejected after ≥ 100 ms (the documented divergence) |
| genuine re-entry through a worker-thread callback | sync: immediate error; async: error after the grace period |
| two sequential acquisitions | `ContentionWait` grants the full grace; the legacy path fails early |

## Re-check on d6 with this transport, applied locally and rebuilt

- `contention_repro.mjs`: build orchestrator 0/3 errors, API checker 0/3.
- `reentry_repro.mjs`:
  - sync `buildDuringBuild`: immediate error;
  - async `buildDuringBuild`: error after 10.3 s;
  - nested snapshot operations: OK.
- The checker lane's `lease_reentrancy` tests: 3/3 pass.

# Review of core 7c34965 (PR 34)

## Inputs

- Core: `7c34965542c4b709a2d9e821b59f2543058ead00`, isolated worktree. It contains runtime 623ee28;
  f0762dc is not included. `target/debug/tsrs` sha256 `16e482d8…5d08`.
- Pinned Go: built at microsoft/TypeScript `b85298b6…` (`cmd/tsc`) as the server, and
  `tsc/cmd/zzoracle` (`tests/fixtures/go_json_oracle_main.go.txt`) for JSON decoding.
- Clients: the pinned `packages/typescript/src/api` at the same commit.
- "c7 + runtime": c7 with this branch's `src/` copied in and rebuilt locally. Not committed to core.

## Params shape probe

All 172 methods, sync, against Go. Artifacts: `c7-shape-probe-{go,tsrs,tsrs-with-runtime}.json`.
Each cell counts responses identical to Go.

| payload | d6 | c7 | c7 + runtime |
| --- | --- | --- | --- |
| lone surrogate / duplicate / nested duplicate / bad UTF-8 / truncated | 171 | 171 | 171 |
| `[1]` (array) | 1 | **171** | 171 |
| `"x"` (string) | – | 171 | 171 |
| `5` (number) | – | 1 | 1 |
| empty payload | – | 1 | **171** |
| `{}` | – | 167 | 167 |
| `null` | – | 50 | 50 |
| wrong field types | 18 | 18 | 18 |

The method missing from each 171 is the profiling methods (`stopCPUProfile`, plus `startCPUProfile`
and `saveHeapProfile` where those params apply). tsrs reports them as unsupported; that is explicit and expected.

### Stable error semantics (class, accept or reject, Go type) still to fix in core

1. **`null` params, 119 methods.** Go decodes `null` into the zero struct, so the handler runs with
   zero fields. Most then fail with `api: client error: …`, for example `snapshot 0 not found`, and
   `createModuleResolver` and `readConfigFile` succeed. tsrs answers
   `api: invalid request: … params must be an object` for all 119.
   - Cause: `dispatch` passes `Value::Null` through, and handlers call `Params::object()` or the
     checker's `params::object()`, which reject null.
   - Fix: in `session.rs` `dispatch`, map `Value::Null` to an empty object for methods that have a
     params type (`methods::params_type(m).is_some()`).
2. **Missing `DocumentIdentifier`.** Go reads a missing identifier as an empty file name (the zero
   value). Affected: `{}` for `readConfigFile` (Go: OK with a 5083 "Cannot read file" diagnostic) and
   `parseConfigFile` (Go: `client error: could not read file "/tmp"`). tsrs says
   `invalid request: DocumentIdentifier: expected string or object`.
   - Fix: `wire.rs` `DocumentIdentifier::parse(Value::Null)` should return
     `FileName(String::new())` for required single identifiers. The checker's `params.rs:165` has
     the same issue.
3. **Field-type errors, 3 methods with the wrong class.** For `retainSourceFile`,
   `getCachedSourceFile` and `resolveModuleName`, Go returns
   `invalid request: failed to unmarshal *api.<T>: json: cannot unmarshal JSON number into Go api.SourceFileDescriptor …`.
   tsrs returns `client error: invalid source file descriptor …`. Type-check the descriptor fields
   before validating them semantically.

### Go decoder wording (variable text, reported separately)

- **Wrong field types:** 148 methods have the same class and the same `failed to unmarshal *api.<T>`
  prefix, but the detail differs. Go: `json: cannot unmarshal JSON string into Go api.SnapshotID within
  "/snapshot"`. tsrs: `json: snapshot must be a non-negative integer`. Reproducing Go's text needs
  Go's Go-type names and pointers for every field.
- **Number params:** Go writes `cannot unmarshal JSON number into Go api.<T>` with no literal; tsrs
  appends the literal (`JSON number 5`). Fix in core's `session.rs`: drop the number text.
- **Syntax wording (transport-owned): fixed on this branch.**
  - `strictjson` now reproduces jsontext's grammar errors: pointer and offset rules, offset 0
    omitted, `at start of value`, `after object value` / `after array element`, `in literal`,
    `in number`, escape snippets, control characters, trailing-comma blame, and member-name errors.
  - Test: `go_syntax_oracle.json` holds 106 inputs decoded by the pinned Go oracle; all 106 match
    (`syntax_errors_match_pinned_go_text`), plus the 38 earlier cases.
  - Through c7 + runtime, 30 of 30 syntax probes on `release` params match Go, and the empty-payload
    row goes from 1 to 171 identical.

## Symlink target notification (`tests/node/requestfs_alias_repro.mjs`, CREATE and UPDATE)

Semantic diagnostics before → after:

| | update / target | update / alias | update / none | create / target | create / alias | create / none |
| --- | --- | --- | --- | --- | --- | --- |
| Go | 1→0 | 1→0 | 1→1 | 1→0 | 1→0 | 1→0 |
| d6 | **1→1** | 1→0 | 1→1 | 1→0 | 1→0 | 1→0 |
| c7 | 1→0 | 1→0 | 1→1 | 1→0 | 1→0 | 1→0 |

c7 fixes UPDATE. CREATE re-reads the file even without a notification, in both Go and tsrs, so CREATE
does not distinguish whether expansion happens. It matches Go, but that is not evidence of expansion
on create.

## Invalid readFile answers during a program build (`tests/node/invalid_callback_repro.mjs`)

Raw sync client: `b.ts`'s readFile is answered with each of these.

| answer | pinned Go | c7 | c7 + runtime |
| --- | --- | --- | --- |
| `{"kind":"bogus"}` | server crash (exit 2), `panic: invalid readFile callback response kind: bogus` | error response, same text | same |
| `"value":5` | crash, `panic: json: cannot unmarshal JSON number into Go string` | error, serde text | **error, Go's text** |
| missing kind / lone surrogate / duplicate kind / truncated / serverFS.error / CallError | crash, `panic: <text>` | error, same text as Go | same |

- **Text:** the panic text is now identical in all 8 cases.
- **Deliberate divergence, explicitly kept:** pinned Go crashes the whole server when a callback fails
  on a compiler worker goroutine (`[recovered, repanicked]`). tsrs answers that request with the
  error and keeps the connection.
- **Also fixed on this branch:** a present `null` readFile value now decodes to `""`, as in Go (it used
  to fail), and an absent value fails with `jsontext: unexpected EOF`, Go's error for an absent value.

## Callback context propagation (guidance for core; no core edits)

This branch adds `RequestScope::current()` / `scope.enter(|| …)`. Client calls inside `enter` are
attributed to that request. With that, worker-thread file reads stop being "unattributed", and the
remaining async divergence (an unrelated waiter rejected after the grace period) goes away for those
reads. Test: `scoped_worker_callback_is_attributed_and_does_not_reject_an_unrelated_waiter`.

Where core can apply it:
1. In `snapshots.rs` `handle_create_snapshot` / `handle_update_snapshot`, and in the build handler,
   capture `RequestScope::current()`.
2. Pass the snapshot host and build a filesystem wrapper whose `FS` methods run
   `scope.enter(|| inner.<op>(..))`:
   - `req.file_system = Some(Arc::new(ScopedFs { inner, scope }))`, layered under any request
     filesystem;
   - and for builds, `BuildRequest.fs`.
3. Do not set `replace_file_system` for the wrapper.
4. Snapshots may keep the wrapper after the request ends. Later lazy reads then count against a
   finished request, which holds nothing, so they never cause a rejection.

# Review of core c2abd68 (PR 34)

## Inputs

- Core: `c2abd684c4d0b0f2df0cc69b5c0f558310a19c86`, isolated worktree. It contains runtime ddaebbc and
  checker f11cc6b. `target/debug/tsrs` sha256 `eb44d50c…4827` (a rebuild reproduced the same hash).
- Pinned Go server: microsoft/TypeScript `b85298b6…`, `cmd/tsc`.
- Raw inputs and outputs: `/root/artifacts/c2-review/`, including the probes (`strict_probe3.mjs`,
  `payload_probe.mjs`, `rmn_cases.json`, `tff.json`).
- Go's decoder randomly says "cannot unmarshal" or "unable to unmarshal": 10 identical runs gave
  9× "cannot" and 1× "unable". Exact-text comparisons below are therefore also given normalized,
  with "unable" treated as "cannot".

## resolveModuleName: the wrong-class payload

Sync request `resolveModuleName` with params `{"snapshot":"x"}` (minimal form; the original probe
payload `{"snapshot":"x","project":7,"file":7,"fileName":7}` behaves the same).

| | answer |
| --- | --- |
| Go | `api: invalid request: failed to unmarshal *api.ResolveModuleNameParams: json: cannot unmarshal JSON string into Go api.SnapshotID within "/snapshot"` |
| tsrs c2 | `api: client error: moduleName is empty` (with `"moduleName":"m"` added: `client error: module resolver 0 not found`) |

The same class mismatch occurs for `{"resolver":"x"}`, `{"resolutionMode":"x"}` and
`{"inProgressSnapshot":-1}`. Cause: `resolveModuleName` validates `moduleName` and looks up the
resolver before it type-checks `snapshot`, `resolver`, `resolutionMode` and `inProgressSnapshot`.

## 172-method inventory (sync), c7 → c2

| payload | identical to Go, c7 | identical, c2 | c2 normalized | same class, c2 |
| --- | --- | --- | --- | --- |
| strictness ×5 (surrogate, duplicate, nested duplicate, bad UTF-8, truncated) | 171 | 171 | 171 | 171 |
| `[1]`, `"x"`, `5`, empty | 1–171 | 1–171 | **171** | 171 |
| `null` | 50 | **169** | 169 | 169 |
| `{}` | 167 | **169** | 169 | 169 |
| `{"zzz":[1,{"a":2}]}` (unknown field) | 167 | 169 | 169 | 169 |
| `{"snapshot":"x"}` | 28 | 30 | 30 | 168 (resolveModuleName) |
| `{"snapshot":"x","project":7,"file":7,"fileName":7}` | 18 | **5** | 5 | **155** |
| `{"file":null}` | 150 | **112** | 112 | **123** |

All rows exclude the 3 profiling methods, which tsrs reports as unsupported.

## Omitted versus explicit null `DocumentIdentifier`: verified independently against Go

- Omitted `file` (`{}`, or `null` params): the zero value. Go and tsrs agree.
- Explicit `"file": null`: Go fails to decode only for the **19 methods whose params struct
  actually has a `file DocumentIdentifier` field**: `formatNodeForInsertion`,
  `getCompletionsAtPosition`, `getConfigSourceFile`, `getDefaultProjectForFile`,
  `getImportAdderEdits`, `getModeForResolutionAtIndex`, `getModeForUsageLocation`,
  `getReferencesToSymbolInFile`, `getResolvedModule`, `getResolvedTypeReferenceDirective`,
  `getSourceFile`, `getSourceFileMetadata`, `getSymbolAtPosition`, `getSymbolOfSourceFile`,
  `getSymbolsAtPositions`, `getTypeAtPosition`, `getTypesAtPositions`, `parseConfigFile`,
  `readConfigFile`.
- For every other method Go ignores the unknown key. For example, the `*FromFile` methods use
  `fileName` and the diagnostics methods use `files`.
- The checker probe's finding (explicit `file: null` is an invalid request) is therefore right only
  for those 19 methods. A blanket "null means empty" rule was wrong; so is c2's blanket rejection.

c2 regressions from c2abd68 (all were correct in c7):

1. **Over-rejection: 40 methods.** tsrs validates a `file` key that the Go struct does not have,
   whether `null` or `7`, and returns `invalid request: … DocumentIdentifier: expected string or
   object`. Go ignores the key. Examples:
   - `release`: Go `client error: empty handle`;
   - `createSnapshot`, `batchRequests`, `transpileModule`, `parseCommandLine`,
     `createBuildOrchestrator`, `createModuleResolver`: Go returns **OK**;
   - `transpileModuleFromFile` / `transpileDeclarationFromFile` / `createSourceFileFromFile`
     (`{"file":null}` or `{"file":7}`): Go `client error: could not read file "/tmp"`;
   - all diagnostics and emit methods: Go `client error: snapshot 0 not found`.

   The full list is in `c2_class_mismatch.json`. This regression also explains the field-type row
   dropping from 18 to 5.
2. **Under-rejection: 8 checker methods.** Decoding still happens after the lookup, so tsrs answers
   `client error: snapshot 0 not found` where Go gives the decode error: `getCompletionsAtPosition`,
   `getImportAdderEdits`, `getReferencesToSymbolInFile`, `getSymbolAtPosition`,
   `getSymbolOfSourceFile`, `getSymbolsAtPositions`, `getTypeAtPosition`, `getTypesAtPositions`.

Fix rule for core: apply the explicit-null `DocumentIdentifier` check only to the
`DocumentIdentifier` fields of the method's own Go params struct (per method, like `PARAMS_TYPES`),
and do it in the decode step, before any snapshot or project lookup. That includes the checker
dispatch.

## Exact wording differences in the same class (kept as-is, reported honestly)

- Field type errors (`{"snapshot":"x"}` and similar), about 140 methods. Go:
  `json: cannot unmarshal JSON string into Go api.SnapshotID within "/snapshot"`. tsrs:
  `json: snapshot must be a non-negative integer`.
- `DocumentIdentifier` errors. Go:
  `json: cannot unmarshal into Go api.DocumentIdentifier within "/file": DocumentIdentifier: expected string or object, got null`.
  tsrs drops the `cannot unmarshal into Go api.DocumentIdentifier within "<ptr>": ` part. For a
  number, Go ends with `got number` and tsrs with `for containingDirectory`.
- `{"file":7}` on `retainSourceFile` / `getCachedSourceFile`: Go `cannot unmarshal JSON number into
  Go api.SourceFileDescriptor within "/file"`. tsrs omits `JSON number`.
- `moduleName: 5`: Go `cannot unmarshal JSON number into Go string within "/moduleName"`. tsrs
  `moduleName must be a string`.

## Real callback attribution after ScopedFs and request-thread builds

Instrumented local rebuild of c2, a 40-file `createSnapshot` plus `build` (`c2_attr.txt`):

| | sync | async |
| --- | --- | --- |
| d6 | 260 of 262 callbacks unattributed | 260 of 262 unattributed |
| c2 | **262 of 262 attributed** | **262 of 262 attributed** (131 on the build request thread, 130 on snapshot-build workers via ScopedFs) |

So the worker-thread case behind the "unattributed" divergence no longer occurs for snapshot and
build reads. An unrelated slow callback is now always counted against its own request, and a waiter
is rejected after the grace period only when the resource holder itself waits on the client for
longer than the grace period. Go waits forever in that case; this remains a bounded divergence.

Real processes at c2:
- `contention_repro.mjs`: build orchestrator 0/3 and API checker 0/3 spurious errors.
- `reentry_repro.mjs`: `buildDuringBuild` gives an immediate error on sync and an error after 10.3 s
  on async; `pingDuringBuild` and in-flight nested snapshots succeed.

# Review of core f70371e (PR 34): generated predecode

## Inputs

- Core: `f70371e5acffd258c3bc5ddbb559c9c51d7d6477`, isolated worktree. `target/debug/tsrs` sha256
  `4d528789…9f29`.
- Pinned Go server: built at `b85298b6…`.
- Raw data and scripts: `/root/artifacts/f7-review/`.
  - `gen_fields.py` derives every params struct's fields and Go types from the pinned `proto.go`:
    170 methods, 505 fields.
- Comparisons treat Go's randomized "unable"/"cannot" as equal. The 3 profiling methods are excluded.

## Results

| check | result |
| --- | --- |
| Shape matrix: 15 payloads × 169 methods (strictness, `[1]`, `"x"`, `5`, empty, `null`, `{}`, unknown key, `{"file":null}`, `{"snapshot":"x"}`, mixed bad fields) | **2535 / 2535 identical**. c2 had 112 for `{"file":null}`, 5 for mixed field types and 30 for `{"snapshot":"x"}` |
| Unknown `file` regression (`{"file":null}`, `{"file":7}` on `*FromFile`) | 7/7 identical |
| `resolveModuleName` payloads (37) | 36 identical, plus one wording difference |
| Field/value matrix, 8837 cases. Each field gets 15 values: `"x"`, 7, -1, 1.5, 1e3, 2^31, 2^32, 2^64, true, [], [1], {}, null, [null], unknown-object. Plus mixed invalid pairs in both orders | 7269 identical; 1149 same class, wording only; 419 class mismatches |
| Ordinary pinned sync and async clients: createSnapshot, getSourceFileNames, semantic/syntactic diagnostics, getSymbolAtPosition (`Dog`), getTypeAtPosition + typeToString (`Dog`), transpileModule, emitToString, updateSnapshot with a layer, release, request after release | all 24 results identical to Go |

## The 419 class mismatches

### Malformed input that tsrs accepts or looks up before rejecting (415)

In these cases Go answers `invalid request` and tsrs answers a later client error, an untyped error
or OK.

| case | count | example |
| --- | --- | --- |
| Exponent-notation integers (`1e3`). Go: `cannot unmarshal JSON number 1e3 into Go api.SnapshotID … : invalid syntax`; tsrs treats it as 1000 | 232 | `release {"snapshot":1e3}` gives `client error: snapshot 1000 not found` |
| Integers out of the field type's range (2^31 for int32, 2^32 for uint32/ResolutionMode, 2^64 for uint64/SnapshotID) | 169 | `resolveModuleName {"resolutionMode":4294967296}` |
| Array element types not predecoded (`[]NodeHandle`, `[]SymbolReference`, `[]ImportAdderAction`, `[]*ReconfigureSnapshotProgramParams`, `[]BatchRequest`, `positions []uint32`) | 14 | `getSymbolsAtLocations {"locations":[1]}` gives `client error: snapshot 0 not found`; `batchRequests {"requests":[1]}` gives **OK** with a per-item error |

### Input Go accepts that tsrs rejects (4)

These are the only cases where a request Go serves fails in tsrs. None appear in normal client traffic.

| case | Go | tsrs |
| --- | --- | --- |
| `batchRequests {"maxResponseBytesPerPage":-1}` (field is `int`) | OK | `maxResponseBytesPerPage must be a non-negative integer` |
| `createBuildOrchestrator {"rootNames":[null]}` | OK (null element becomes "") | invalid request |
| `parseCommandLine {"commandLine":[null]}` | OK | invalid request |
| `createSnapshot {"ensurePrograms":[null]}` | OK | invalid request |

A JS client produces `[null]` when an array holds `undefined`.

## Wording-only differences (1149)

All are in the same class, and none change whether a request is accepted.

- **594: number literal omitted.** For a non-integer, out-of-range or negative-for-unsigned number,
  Go says `cannot unmarshal JSON number 1.5 into Go int … : invalid syntax`. tsrs omits the literal
  and the `: invalid syntax` suffix.
- **About 280: field order with several invalid fields.** Go reports the first invalid field in
  *document* order; tsrs reports the first in *struct declaration* order. Example:
  `batchRequests {"continuationToken":[{"bad":1}],"requests":"bad"}`. Go reports
  `/continuationToken`, tsrs reports `/requests`.
- **215: package qualifier.** `[]api.BatchRequest` / `[]api.DocumentIdentifier` versus tsrs's
  `[]BatchRequest`.
- **15: alias names.** `core.ResolutionMode` is an alias of `core.ModuleKind`, and Go prints
  `core.ModuleKind`.
- **17: own messages.** Some custom-unmarshaler types have their own text, for example
  `removePrograms` / `ensurePrograms` (`cannot unmarshal into Go project.SyntheticProjectID within
  "/removePrograms/0": …`), and a nested element (`createPrograms [1]`) gives `params must be an
  object`.
- **1: `resolveModuleName {"inProgressSnapshot":-1}`.** Same as the number-literal case.

Nested struct decode order and exact jsontext wording inside nested structs remain documented gaps.

# Review of core b2769b8 (PR 34): integer ranges, array elements

## Inputs

- Core: `b2769b86a49a28c8e78a3aa011cb6b223fd4458a`, confirmed with `git ls-remote` before fetching.
  `target/debug/tsrs` sha256 `77d26329…e2b3`.
- Pinned Go: `b85298b6…`, binary sha256 `98159140…eacc`.
- Raw data and scripts: `/root/artifacts/b2-review/`.
  - `payload_probe2.mjs` sends the payload bytes verbatim, over sync MessagePack or as the `params`
    text of a JSON-RPC frame.
- Comparisons treat Go's "unable"/"cannot" as equal. The 3 profiling methods are excluded.

## Source concern 1 confirmed: integers are range-checked and passed on as f64

Results were identical over sync and async.

| payload | pinned Go | tsrs b2769b8 | effect |
| --- | --- | --- | --- |
| `release {"snapshot":9007199254740993}` (2^53+1) | `client error: snapshot 9007199254740993 not found` | `snapshot 9007199254740992 not found` | **looks up the wrong ID** |
| `release {"snapshot":9223372036854775807}` (i64 MAX) | `snapshot 9223372036854775807 not found` | `snapshot 9223372036854775808 not found` | wrong ID |
| `release {"snapshot":18446744073709551614}` / `…615` (u64 MAX-1 / MAX) | `snapshot … not found` (valid u64) | invalid request | **rejects a value Go accepts** |
| `release {"snapshot":18446744073709551616}` (u64 MAX+1) | invalid request (`: value out of range`) | invalid request | same class |
| `batchRequests {"maxResponseBytesPerPage":9223372036854775807}` (int, i64 MAX) | OK | invalid request | **rejects a value Go accepts** |
| `batchRequests {"maxResponseBytesPerPage":-9223372036854775809}` (i64 MIN-1) | invalid request (`value out of range`) | **OK** | accepts out-of-range input |
| `-9223372036854775808`, 2^53-1, 2^53, u32 MAX, i32 MIN/MAX | same as Go | same | |
| `position 4294967296`, `flags ±2147483648/9` | invalid request | invalid request | wording only |

Cause: `predecode.rs` `int_ok` compares a rounded f64 against the range. `wire::as_u64` then casts
the rounded f64. Core fix: parse the raw decimal lexeme as i64/u64 exactly, both for the range check
and for the value that gets used. For example, `lexeme.parse::<u64>()` / `parse::<i64>()` on lexemes
with no `.`, `e` or `E`.

## Source concern 2 confirmed: escaped keys bypass the raw-lexeme check

Results were identical over sync and async.

| payload | pinned Go | tsrs |
| --- | --- | --- |
| `release {"snap\u0073hot":1e3}` | invalid request (`1e3 … invalid syntax`) | **`client error: snapshot 1000 not found`** |
| `releaseSourceFile {"le\u0061se":1e3}` | invalid request | **`client error: source file lease 1000 not found`** |
| `getSymbolsAtPositions {"positi\u006fns":[1e3]}` | invalid request | **`client error: snapshot 0 not found`** |
| `release {"snap\u0073hot":9007199254740993}` | `snapshot 9007199254740993 not found` | `…992` (concern 1) |
| `release {"snap\u0073hot":-1}`, `{"positi\u006fns":[4294967296]}` | invalid request | invalid request (the sign and range check still runs on the decoded value) |
| `release {"snap\u0073hot":"x"}`, `{"snap\u0073hot":1}`, `{"loc\u0061tions":[1]}`, `{"requests":[{"meth\u006fd":"ping"}]}` | same as Go | same | |

Cause: `number_lexemes` keys the raw lexemes by undecoded key bytes, while parsed params use decoded
keys. So when a key contains an escape, no lexeme is found and the exponent/fraction check is skipped.
Core fix: decode JSON escapes in the lexeme map's keys, the way `strictjson` already decodes names.
The transport delivers the payload bytes unchanged in both modes, so no transport change is needed.

## Full matrices at b2769b8

| check | f70371e | b2769b8 |
| --- | --- | --- |
| Shape matrix, 15 payloads × 169 methods | 2535/2535 | **2535/2535** |
| Field/value matrix, 8837 cases: identical | 7269 | 7288 |
| … same class, wording only | 1149 | 1548 |
| … class mismatches | 419 | **1** |

The matrix's values are `1e3`, 2^31, 2^32 and 2^64. It does not include escaped keys or values
between 2^53 and 2^64 in range; those are covered by the targeted table above.

- **Remaining class mismatch:** `createSnapshot {"removePrograms":[null]}`.
  - Go: invalid request (`cannot unmarshal into Go project.SyntheticProjectID within "/removePrograms/0": invalid synthetic project ID: `).
  - tsrs: `client error: failed to create snapshot: synthetic program not found for removal: `.
  - It also uses up a snapshot ID, so every later `createSnapshot` in the same session returns an ID
    one higher than Go's. Seen in the matrix as `"snapshot":16` versus Go's `15`.
- **Wording-only differences (1548):**
  - 872: number literal and Go's `: invalid syntax` / `: value out of range` suffix omitted. This
    grew because the out-of-range classes now match;
  - 277: with several invalid fields, Go reports document order, tsrs struct order;
  - 215: `api.` package qualifier missing from Go type names;
  - 45: `core.ModuleKind` alias name;
  - 139: custom or nested messages. Examples: `ensurePrograms`/`removePrograms` text, `[]api.DocumentIdentifier`,
    and the ID drift above showing up in OK bodies.

## Real sync and async array/batch smoke (`array_batch_flow.mjs`, pinned clients)

The async client sends the concurrent array requests as one `batchRequests`. Calls covered:
`getSymbolsAtPositions` and `getTypesAtPositions` (6 positions), `getSemanticDiagnostics` and
`getSyntacticDiagnostics` with `files` arrays, `getTypesOfSymbols` with 6 symbol references, 6×
`typeToString`, and `release`. Result: 7/7 steps identical to Go in both modes, for example
`["Dog","string","number","Dog","(n: number) => string","number"]`.
