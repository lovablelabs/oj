# Node API checker lane

Port of the checker / symbol / type / signature request handlers of the tsgo API
(microsoft/TypeScript at the workspace Cargo pin `3e9236361fa97a9e6c685f4833ac457ae2d8c50a`,
`tsc/internal/api/session.go` and `proto.go`). Wire shapes,
id ownership and nullability follow the Go structs and handlers one to one;
there is no separate "tsrs subset" API.

## Contract with the core session

The core lane owns the crate root, snapshots, source-file leases, the AST encoder / node index table and
dispatch. It integrates this module through:

```rust
// in core's request dispatch (crate-private; it wraps the session in `SessionHost`, the `CheckerHost` impl):
if let Some(result) = checker::handle(session, method, &params) { /* map result */ }
```

* `CheckerHost` (checker/host.rs) — implemented by core:
  * `context()` — request `Context`; handlers add `CheckerLifetime::API` and acquire the program's
    persistent API checker exactly once per request (exclusive, not reentrant; no callbacks run while
    it is held).
  * `snapshot(handle) -> SnapshotScope` — keeps the snapshot (programs, checker pools, source files)
    alive until the scope drops; carries that snapshot's `Arc<CheckerRegistry>`.
  * `acquire_cached_source_file(descriptor)` — Go `acquireCachedSourceFile` for file-owned symbol
    references that carry no snapshot.
  * `source_file_descriptor(file)` / `source_file_node_id(file)` — Go `newSourceFileDescriptor` /
    `sourceFileNodeID` (must match what `getSourceFile` responses report).
  * `node_handle(node)` / `resolve_node_handle(program, handle)` — Go `nodeHandleFrom` /
    `resolveNodeHandle` (`"<index>.<kind>.<path>"` over the encoder's node index table).
  * `encode_node(node)` — Go `encoder.EncodeNode(node, nil)` for `typeToTypeNode` /
    `signatureToSignatureDeclaration`; the handler returns `CheckerResponse::EncodedNode(bytes)` and the
    transport emits raw binary (msgpack) or `{ "data": base64 }` (JSON-RPC).
* `CheckerRegistry` (checker/registry.rs) — one per API snapshot handle, stored in core's snapshot data.
  Core must call `registry.release()` before releasing the snapshot's last API reference. After release
  every lookup is a client error; no raw pointer outlives its snapshot.
* Errors: `CheckerError { kind: InvalidRequest | Client | Unsupported | Internal, message }` = Go
  `ErrInvalidRequest` / `ErrClientError` / an explicit unsupported error / other errors (mapped to core's
  `ErrorKind` in checker.rs). Handlers do not panic on bad input (wrong JSON types, out-of-range numbers, wrong
  type kind for a property, stale/foreign handles); panics from deep checker invariants are not caught
  here; core's `Session` converts them to a `panic: ...` error response like Go's `recover` in the ipc loop.

## Id ownership (same as Go)

* Symbol ids: `tsrs_ast::get_symbol_id` (process-wide counter, not an address). Non-transient symbols of
  non-content-mapped files are *file-owned* (`reference.kind = 0`, carries the source-file descriptor) and
  are resolved through the file, never through a registry; all others are *snapshot-owned*
  (`kind = 1`, snapshot + canonical project) and registered snapshot-wide.
* Type / signature ids: per-checker ids, registered per (snapshot, project), and pinned to the id of the
  API checker that produced them. A replaced API checker (e.g. after cancellation) makes older handles a
  stale-handle error instead of resolving against another checker. Cross-project and cross-snapshot
  handles are rejected.
* Positions in requests are UTF-16 offsets, converted with the file's position map before
  `GetTouchingPropertyName`.

## Coverage

`crates/tsrs_api/tests/integration/checker/coverage_table.rs` has one row per `Method` constant in ported `proto.go`
(177 through source 29, same order as core's `methods.rs`). 119 are owned by this lane (the 115 checker/symbol/type/signature methods plus the four
language-service backed ones: `getCompletionsAtPosition`, `getReferencedSymbolsForNode`,
`getSignatureUsages`, `getImportAdderEdits`). `crates/tsrs_api/tests/integration/checker_test.rs` checks the table
against core's inventory and `ts-ref/tsc/internal/api/proto.go` (when checked out) and that every
`Implemented`/`Tested` row is dispatched.

Status today: all 119 are `Tested` = dispatched and exercised by a real-program Rust test (real
`tsrs_project` snapshot of a tsconfig project + the program's persistent API checker; fixtures cover
generics, unions, aliases, overloads, `.d.ts` imports, labeled tuples, mapped / conditional /
substitution / template-literal types, enums, JSDoc, unicode identifiers and UTF-16 positions after
non-BMP text, plus stale / cross-project / released / malformed handle validation).

Source 29 adds `getMergedSymbol`, `getSymbolOfNode`, `getSymbolOfDeclarationForChecker`, and
`getParentOfSymbolForChecker`. They use the checker and handle registry for the specified snapshot/project;
the node/declaration and parent methods may return `null`. A focused real-program Rust test covers merges,
missing symbols, parent lookup, and late-bound symbols, and upstream sync/async API cases exercise the
client path. These four methods are not in the pinned-base Go golden fixtures below.

What `Tested` does **not** mean yet (open gaps, for the integration lead):

* `pinned_go_differential` compares 71 results with `testdata/go_probe/go_probe_b85298b6.jsonl`. The
  file is recorded from pinned Go's `Session.HandleRequest` (built from b85298b6, go1.27.1). Regenerate
  it with `testdata/go_probe/regen.sh`. The script checks that `ts-ref` is clean and at the commit pinned
  in `Cargo.toml`, copies `zz_tsrs_probe_test.go` into it for the run only, and removes it again,
  failing if the checkout is not clean afterwards. Fixture and needles are shared with the Rust side
  (`testdata/go_probe/fixture`, `needles.txt`).

  The comparison covers type strings, type flags and objectFlags at 40 positions, symbol names, flags
  and ownership, file-owned lookups without a snapshot (parent, members, exports, export symbol),
  `resolveName` with and without a location, type-parameter constraints, and stale, foreign-project,
  forged and released handle errors. Results are compared as JSON values, not bytes, with two
  explicit exceptions:
  * objectFlags ignore only `MembersResolved` (1 << 21). It is a lazily set cache bit; Go itself
    reports 536870916 vs 538968068 for `box` at two positions, and 16 vs 2097168 for `make`. The raw
    differences from tsrs are only in that bit: `doubled` and `arr` (Go 2621444, tsrs 524292) and
    `box.value` (Go 538968068, tsrs 536870916). No other objectFlags bit differed.
  * For a malformed `type` param, both return `api: invalid request: ...`, but the wording differs.
    tsrs says `field "type": expected an unsigned integer in range`; Go prints its json/v2 decoder
    error, whose wording itself changes between Go processes ("unable to" vs "cannot unmarshal"; observed on
    regeneration, so the committed line is whichever run produced it).

  This is a Go-server comparison through tsrs' test-only node table. It is not a Node-client or
  `Session` run; the broad upstream client suite belongs to the parity lane.
* `session_responses_match_pinned_go` (checker/session_tests.rs) runs through core's real `Session`, with
  codec node handles, source-file descriptors and leases. It compares 84 full responses with
  `testdata/go_probe/go_shapes_b85298b6.jsonl`, recorded by `TestTsrsCheckerShapes` in the same probe.
  All 84 match after normalizing per-process counters: type, signature and symbol ids, source-file node
  ids, and the symbol id embedded in `__@iterator@<id>` names. Node handles, content hashes, flags and
  error texts are compared as is. It pins:
  * **encoding/json v2 rules.** `omitempty` drops only null, "", [] and {}. So `objectFlags: 0`,
    `isTupleType`, `isThisType` and `isReadonly: false` are always sent, nil slices are `[]` (not
    `null`), and completion `kind: 0` is sent.
  * **Wrong-kind requests.** Pinned Go does not validate these; it panics and the connection reports
    `panic: <value>`. tsrs returns the same first line as an error without panicking. Go answers some
    of them normally (`getBaseTypes`, `getConstraintOfTypeParameter`, `getDefaultFromTypeParameter` on
    a literal; `getSignaturesOfType` with an unknown kind), and so does tsrs.
* `testdata/node/identity.test.ts` is a probe for the pinned upstream **sync** client (not part of the
  parity suite). It checks cross-request object identity, symbols and types from a retained old snapshot
  after an update, disposal in both orders, and old-snapshot checker requests issued from inside a
  `readFile` callback of `update()`.
  * **How to run.** Copy it into a `tools/node-api` harness work tree as
    `packages/typescript/test/checker/`, then run
    `node --conditions @typescript/source --test test/checker/identity.test.ts` with `CHECKER_OBS_OUT`
    and `CHECKER_REENTRY_OUT` set. Run it once with the Go oracle as `built/local/tsc` and once with tsrs,
    then compare the two JSON files.
  * **Result on core d6d8e2a.** Identical output, 3/3 runs each.
  * **Shared behavior worth knowing** (both servers):
    * Type handles carry no snapshot. An old snapshot's type id used with the new snapshot's checker
      resolves to whatever type the new checker registered under the same number. In the probe that is
      even the "same" type (`Box<string>`), or another type entirely (`string`).
    * A file-owned symbol of an unchanged file keeps its client object and id across snapshots. A
      recreated snapshot re-parses the file, so old references are "not part of the requested program".
* **API checker re-entrancy** (`lease.rs`). This is a deliberate divergence: pinned Go blocks forever if
  a request issued from inside a client callback needs the API checker that the callback's own request
  holds. tsrs takes a per-program gate before the checker slot, using the runtime's holder-aware
  contention API (`tsrs_api_transport::{Holder, ContentionWait}`, runtime f0762dc or later):
  * The holder records `Holder::current()` when it takes the gate and clears it on release.
  * An uncontended request proceeds immediately.
  * A contended acquisition uses one `ContentionWait` for its whole wait, updating the holder whenever it
    changes, and waits like Go while the holder makes progress.
  * It fails, with the transport's bounded re-entrancy error, only when the holder (or what the holder
    waits for) has an attributed client call in flight, or when an unattributed call is in flight. On a
    sync connection that is immediate; on an async one it is after the connection's grace period,
    measured for this acquisition only.
  * Checker requests that do not need the API checker are never rejected.

  `lease_tests.rs` covers this over real connections. The grace is configurable with
  `TSRS_CHECKER_TEST_GRACE_MS` (default 300 ms). Cases:
  * genuine re-entry: sync rejects at once; async rejects after the full grace period, while a lease-free
    request on the same connection still succeeds;
  * ordinary contention that lasts longer than the grace period still succeeds;
  * an unrelated request's attributed callback does not reject a legitimate wait;
  * two sequential waits in one request each get the full grace period;
  * a waiter blocked behind a holder exits when the connection closes.

  The unrelated-callback and two-waits tests fail with the legacy `blocking_may_deadlock()` predicate.
* Node handles, source-file descriptors / leases and AST encoding come from core + `tsrs_api_codec`
  (wired in core's integration candidate a9b4354); `session_tests.rs` exercises them through `Session`,
  including snapshot-less file-owned lookups (`getMembersOfSymbol` / `getExportsOfSymbol` / `getParentOfSymbol`).
* Panics from deep checker invariants are converted to errors by core's `catch_unwind`; the API checker
  is not discarded afterwards (Go keeps it too).
