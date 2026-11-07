# Resident compiler protocol

Run `oj stylex serve --stdio --threads 8` (or `fru serve ...`). Send one JSON
request per line on stdin; stdout returns one JSON response per line. Responses
carry the request's `id` and can arrive out of order. All diagnostics stay on stderr or in error replies.

```json
{"id":1,"op":"hello"}
{"id":2,"op":"transform","root":"/app","filename":"src/badge.ts","source":"import * as stylex from '@stylexjs/stylex'; export const s = stylex.create({base:{color:'red'}});","options":{"dev":false,"runtimeInjection":false},"key":"/app/src/badge.ts","wantCode":false}
{"id":3,"op":"sheet","config":{}}
```

Wait for the transform response before requesting a sheet that must include it.
`hello` returns a compiler `fingerprint`, process ID and worker count. The
fingerprint starts with the compiler version followed by `+`; clients should
reject incompatible versions.

`transform` returns `code` and StyleX metadata `rules`. `wantCode: false` omits
the code; `wantMap: true` requests a source map. Supplying a `key` also replaces
that file's rules in the resident registry and returns `generation` and
`rulesChanged`. Without a key, the transform leaves the registry unchanged.

`sheet` returns the assembled `css`, registry `generation` and file count.
`removeFile` takes a `key` and removes its rules. `setFileRules` takes a `key`
and `rules` to register previously computed metadata. A failed transform does
not replace the file's last successful rules.

Errors return `ok: false`, an `errorClass` and a `message`. Successful responses
return `ok: true`. An absent right-to-left rule is omitted from its metadata
object. Close stdin after outstanding replies have arrived to stop the process.

The complete Rust request schema and implementation are in
[`src/cli/serve.rs`](../src/cli/serve.rs).
