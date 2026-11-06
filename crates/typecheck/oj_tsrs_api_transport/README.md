# tsrs_api_transport

Transport for the tsgo-compatible programmatic API (`tsrs --api`, `tsrs --api --async`), ported from
microsoft/TypeScript at the commit pinned in the workspace `Cargo.toml`
(`[workspace.metadata.typescript].commit`). Upstream sources (Apache-2.0, see the repository `NOTICE`):

| this crate | pinned upstream |
| --- | --- |
| `msgpack.rs` | `tsc/internal/api/protocol_msgpack.go` |
| `jsonrpc.rs` | `tsc/internal/jsonrpc/baseproto.go`, `jsonrpc.go`, `tsc/internal/ipc/protocol_jsonrpc.go` |
| `conn_sync.rs` | `tsc/internal/ipc/conn_sync.go` |
| `conn_async.rs` | `tsc/internal/ipc/conn_async.go` |
| `timing.rs` | `tsc/internal/ipc/timing.go` |
| `transport.rs` | `tsc/internal/ipc/transport*.go`, connection setup in `tsc/internal/api/server.go` |
| `callbackfs.rs` | `tsc/internal/api/callbackfs.go` |

## Contract with the API session

- `Handler::handle_request(cx, method, params) -> Result<Response, ApiError>`: `params` is the raw
  payload (JSON text, or empty). `Response::Json(bytes)` must be one JSON value (validated before it is
  written; an invalid value becomes an error response instead of a corrupt frame). `Response::Binary`
  is `api.RawBinary`: verbatim tuple payload in MessagePack mode, a base64 JSON string under JSON-RPC
  (what Go's `json.Marshal([]byte)` produces). `ApiError { code, message }`: the client sees `message`
  (MessagePack `Error` tuple payload / JSON-RPC `error.message`). Session errors use
  `CODE_INTERNAL_ERROR`, as `ipc.handleRequest` does. Panics are caught and reported as `panic: <msg>`.
- `getServerTiming` / `resetServerTiming` are answered by the connection (`ConnOptions.collect_timing`
  = `--timing`), never by the handler.
- `Caller::call(method, params)` (server-to-client) is obtained before the connection runs
  (`LateCaller`, Go's `SetConnection`) and can be used from any thread while a request is in flight.
  - Sync (MessagePack): calls are serialized; the response is read inline. A client request that
    arrives while waiting (the client re-entering the API from a callback) is dispatched on the waiting
    thread with the protocol lock released and `RequestContext.depth >= 1`. A handler must not hold a
    non-reentrant resource (an exclusive checker lease, a session mutex) across a callback if a nested
    request could need it: reject such nested requests with an error instead of blocking.
  - Async (JSON-RPC): every request runs on its own thread (Go: goroutine), responses are routed by
    `"api<N>"` IDs, and no lock is held while waiting.
- Run ends on EOF (`Ok`) or a fatal transport error. On exit the `CancellationToken` is set, pending
  calls fail with `ipc: connection closed`, and (async) all handler threads are joined.
- stdout is the wire. Nothing else in the process may write to stdout once the transport starts;
  diagnostics go to stderr.

## Adapter for core (`crates/tsrs_cli/src/api.rs`, owned by core)

`tsrs_api::handler` (core) and this crate have the same shape; the CLI adapts them (this crate does not
depend on `tsrs_api`; `tsrs_api` uses this crate's `strictjson` and `reentrancy` modules):

```rust
struct SessionHandler(Arc<tsrs_api::Session>);
impl tsrs_api_transport::Handler for SessionHandler {
    fn handle_request(&self, _cx: &RequestContext, method: &str, params: &[u8]) -> Result<transport::Response, transport::ApiError> {
        match self.0.handle_request(method, params) {
            Ok(tsrs_api::Response::Json(s)) => Ok(transport::Response::Json(s.into_bytes())),
            Ok(tsrs_api::Response::Binary(b)) => Ok(transport::Response::Binary(b)),
            Err(e) => Err(transport::ApiError::internal(e.to_string())), // Go: CodeInternalError + err.Error()
        }
    }
    fn handle_notification(&self, _cx: &RequestContext, method: &str, params: &[u8]) {
        let _ = self.0.handle_notification(method, params);
    }
}
struct ClientConnAdapter(Arc<dyn tsrs_api_transport::Caller>);
impl tsrs_api::handler::ClientConn for ClientConnAdapter {
    fn call(&self, method: &str, params: &str) -> tsrs_api::ApiResult<String> {
        let raw = self.0.call(method, Some(params.as_bytes())).map_err(|e| ApiError::internal(e.to_string()))?;
        String::from_utf8(raw).map_err(|e| ApiError::internal(e.to_string()))
    }
}
// main (server.go Run):
let options = ServeOptions { protocol, collect_timing: timing, max_frame_bytes: DEFAULT_MAX_FRAME_BYTES };
tsrs_api_transport::serve(pipe.as_deref(), options, |caller| {
    let mut fs: Arc<dyn FS> = Arc::new(bundled::wrap_fs(osvfs::fs()));
    if !callbacks.is_empty() || case_sensitive.is_some() {
        let cb = Arc::new(CallbackFs::new(fs, &CallbackConfig::parse(&callbacks)?, case_sensitive));
        cb.set_connection(caller.clone());
        fs = cb;
    }
    let session = Session::new(SessionOptions { fs, binary_responses: !is_async, .. });
    session.set_connection(Arc::new(ClientConnAdapter(caller)));
    Arc::new(SessionHandler(session))
})
```

`--callbacks` is a comma-separated list (`CallbackConfig::parse`; unknown names are an error).

## Re-entrancy (note for core: session locks and checker leases)

A client callback may call the API while the request that triggered it is still running. Sync mode
handles that nested request on the thread blocked in `Caller::call` (`depth >= 1`); async mode handles
it on another thread. If the nested request blocks on something the waiting request holds (a session
mutex, an exclusive checker lease, `languageServerUpdateMu`-style locks), the pinned Go server hangs.
tsrs policy: never hang; succeed when no held resource is needed (as Go does), otherwise fail fast with
`api: client error: <resource> is in use by a request that is waiting on a client callback; ...`.

What the transport provides (no change to core's `Handler` signature needed):

- `tsrs_api_transport::current_request()` (thread-local `RequestContext`: `depth`, `cancel`,
  connection-wide `callbacks.waiting_on_client()`), valid inside `handle_request` on both conns.
- `lock_for_request(&Mutex<T>, "resource name")`: blocks while the holder makes progress, returns the
  error above when the lock is contended while a request on the connection waits for the client, and is
  a plain lock outside requests.
- `Holder::current()` / `ContentionWait`: for a non-mutex exclusive resource (the checker lease), record the
  holder on acquisition and create one `ContentionWait` per contended acquisition; when `may_deadlock()` is
  true, return `reentrancy::reentrancy_error(..)` instead of waiting. `blocking_may_deadlock()` is the legacy
  predicate, kept for compatibility; it can inherit a previous wait's grace start.

Core (owns the session and adapter): take session-wide locks and exclusive checker leases through these
helpers in any path that can reach the callback filesystem, or release them before filesystem access.
`tests/integration/reentrancy.rs` proves the policy with a session-shaped handler on both protocols (nested request
needing the held lock errors, nested request not needing it succeeds at depth 1, plain contention waits,
closing while a handler waits on a callback returns and releases the lock), all under a 20 s watchdog.
Handlers that ignore the cancellation token can still delay `run`'s final join (Go also waits).

## Request filesystem

Request filesystem layering belongs to `tsrs_api::requestfs`, the implementation used by the API session.
Its Go-fixture differential and real-filesystem tests run without feature flags in
`crates/tsrs_api/tests/integration/requestfs_differential.rs`. This transport crate validates the raw JSON before dispatch.

## Strict JSON (`strictjson`)

The pinned decoder rejects duplicate member names anywhere in a document (even inside fields it
ignores), unpaired surrogate escapes and invalid UTF-8 — nothing is replaced with U+FFFD.
`decode_message` (JSON-RPC: a violation is a fatal read error, as in Go, so a lone surrogate in an async
callback result ends the connection) and the callback filesystem (the triggering request fails with
`panic: jsontext: ...`) apply it. Session params decoding is core's: it reads request params with `strictjson::decode`, which validates
and builds the value in one scan, so duplicates are rejected like Go's `unmarshalPayload`.

## Tests

- `tests/integration/go_frames.rs`: byte-exact writes and reads against frames produced by the pinned Go code.
- `tests/integration/conn.rs`: real OS pipes: split frames, frame limits, callbacks, nested requests, remote
  errors, EOF with calls pending, protocol violations.
- `tests/integration/node_roundtrip.rs` + `tests/node/roundtrip.test.mjs` (15 node tests): the pinned Node `sync`
  and `async` clients (from `ts-ref`, `node --conditions=@typescript/source`) against
  `examples/transport_test_server.rs`, including fs callbacks with every serverFS sentinel, callback
  re-entry, astral text, lone surrogates (rejected like Go), 5 MiB payloads, timing, server exit, and
  `--pipe` sockets. On Unix `--pipe` creates a private directory containing a `socket` file; on Windows it
  creates a named pipe restricted to the current user. Mandatory: it fails with install instructions when node
  or ts-ref is missing.
  Rust-only: `cargo test -p tsrs_api_transport --test integration -- --skip node_roundtrip`.
- `tests/integration/strictjson_oracle.rs`: 38 inputs decoded by a Go binary built at the pinned commit
  (duplicate names at any depth, lone/unpaired surrogates, invalid UTF-8, valid astral pairs): same
  accept/reject decisions and identical error text for callback responses, JSON-RPC messages and
  request-filesystem params.
- `tests/integration/reentrancy.rs`: see Re-entrancy.

## Deliberate differences from Go

- Frames are bounded (`ServeOptions.max_frame_bytes`, default 1 GiB) and allocated as bytes arrive,
  never from the declared length; header lines are capped at 8 KiB.
- A frame truncated after its first byte is an explicit `UnexpectedEof` error; Go reports some of
  these as a clean `io.EOF`.
- JSON syntax error texts come from serde_json, not encoding/json/v2.

## Not supported

- `$/cancelRequest` and other notifications are ignored, exactly as the pinned session does.
- External content-mapper spawning (`--runExternalCode`) is not part of this crate.

## Fixtures

`tests/fixtures/go_frames.json` was produced by running `tests/fixtures/go_frames_gen.go.txt` as a Go
test in a package next to the pinned `protocol_msgpack.go` (renamed package) inside the pinned
`tsc` module: `GOWORK=off FRAMES_OUT=... go test ./internal/zzframes/`.
