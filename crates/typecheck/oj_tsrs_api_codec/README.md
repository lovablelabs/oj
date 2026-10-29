# tsrs_api_codec

Binary AST codec of the TypeScript native API (`--api`): a Rust port of
`microsoft/TypeScript` `tsc/internal/api/encoder`. The workspace tracks source commit
`3e9236361fa97a9e6c685f4833ac457ae2d8c50a`; the checked-in codec goldens were recorded during the
upgrade at `2f9fd09a104ef14268944a0aeb9cdc4d80abb6a6`. The codec produces the reference Go server's bytes for
`getSourceFile` / `createSourceFile` / `typeToTypeNode` / …, which the reference JavaScript client
(`packages/typescript/src/api/node`, `RemoteSourceFile`/`RemoteNode`) decodes lazily, and decodes the bytes the
client sends back (`printNode`, `formatNode`-style requests). It does not define a new format.

Derived from TypeScript (Apache-2.0, Copyright (c) Microsoft Corporation); see the repository `NOTICE`.

## API

| Rust | Reference Go |
| --- | --- |
| `encode_source_file(&'static SourceFile) -> (Vec<u8>, NodeIndexTable)` | `EncodeSourceFile` |
| `encode_node(P<Node>, Option<&'static SourceFile>) -> (Vec<u8>, NodeIndexTable)` | `EncodeNode` |
| `build_node_index_table(&'static SourceFile) -> NodeIndexTable` | `BuildNodeIndexTable` |
| `NodeIndexTable::{node(i), get_index(n)}` | `NodeIndexTable.Nodes[i]`, `GetIndex` |
| `set_source_file_id`, `set_source_file_lease`, `source_file_hash` | `SetSourceFileID`, `SetSourceFileLease`, `SourceFileHash` |
| `decode_nodes(&[u8]) -> DecodedNode` (owns a fresh `Region`) | `DecodeNodes` |
| `decode_source_file(&[u8]) -> DecodedNode` | `DecodeSourceFile` |
| `decode_nodes_in_current_region(&[u8], &NodeFactory) -> P<Node>` | `DecodeNodes` into the caller's region |
| `PositionMap` (WTF-8 aware UTF-8 ⇄ UTF-16) | `ast.PositionMap` |
| `node_handle`, `parse_node_handle`, `resolve_node_index` | session.go `nodeHandleFrom`, `resolveNodeHandle` (parsing/index part) |

Node handles on the wire are `"<index>.<kind>.<path>"` (session.go `nodeHandleFrom`); `index` is the
`NodeIndexTable` index. The session owns building/caching the table per source file and must drop it together
with the snapshot/lease that keeps the file's region alive: the table holds raw `P<Node>` arena pointers and
the codec never extends their lifetime. `DecodedNode::root()` is valid only while the `DecodedNode` lives.

## Generation

`node crates/tsrs_api_codec/gen/gen-codec.ts` regenerates `src/generated.rs` and `COVERAGE.generated.md` from
`ts-ref/tools/scripts/tsc/ast.json` through `tools/gen-ast/schema.ts` (the same inputs as `tsrs_ast`), porting the
classification of the reference `tools/scripts/tsc/generate-encoder.ts` (data type, children masks in visitor
order, 6-bit commonData layout incl. SyntaxKind-union indices, string/extended data, decoder factories with
kind-alias deconfliction). `--check` fails if the checked-in files are stale. The traversal is tsrs_ast's
port of Go's `NodeVisitor.VisitEachChild` with the encoder's `VisitNodes`/`VisitModifiers` hooks plus
`node.jsdoc(file)`, so record order, NodeList records, empty-modifier elision and the unmasked
`FullSignature` child match Go.

## Verification

The checked-in goldens were recorded with `ts-ref` at source commit `2f9fd09a104ef14268944a0aeb9cdc4d80abb6a6`,
the Go server built from that revision with Go 1.27.0, and the `npm/tsrs` JavaScript client compiled
from source-24 SDK files. `tests/golden/manifest.json` records the source SHA, Go version, executable
sha256 and client package. To refresh them, verify and build the current Cargo-pinned Go reference using
[the reference workflow](../../CONTRIBUTING.md#reference-compiler), build the matching local SDK, then run
from the repository root:

```sh
TSRS_CODEC_TSC="$PWD/target/reference/tsgo" TSRS_CODEC_ORACLE=npm/tsrs vp run gen:codec-oracle
vp run gen:codec-probe
```

`cargo test -p tsrs_api_codec` (`TSRS_CODEC_ORACLE=npm/tsrs` enables the Node test,
`TSRS_CODEC_TSC=<reference tsc>` additionally its live printNode check):

* `golden_compat`: the Rust encoding of each of the 12 public fixtures (`tests/fixtures`: TS, TSX, `.d.ts`,
  checked JS with every JSDoc tag form, BOM + CRLF + astral identifiers, escapes producing WTF-8 lone
  surrogates, parse errors, missing declarations, empty file) is byte-identical to the reference server's
  `createSourceFile` response (ID/lease zeroed): header, string table, extended data, msgpack structured data
  and every node record. The fixtures contain every node kind a parser produces (236 kinds). `GetIndex` matches
  the reference Go result for all 1625 node records (121 of them resolve to another occurrence of a node that is
  encoded several times, i.e. a JSDoc comment hosting `@typedef`/`@callback` attached to several declarations);
  index tables rebuilt later agree. Plus node handles and generator freshness.
* `decoder`: decodes the server's and the JS client encoder's bytes and compares structure; Go
  `DecodeNodes`+`EncodeNode` (reference probe) vs Rust `decode_nodes`+encode on 30 inputs (all server goldens and
  all client-encoded print inputs, incl. synthesized trees with 0xFFFFFFFF positions and SourceFile roots
  mixing synthesized and parsed nodes): 29 byte-identical, 1 identical failure (see below); ~13k malformed
  inputs never panic.
* `print_parity`: reference server `printNode` vs `decode_nodes` + `tsrs_printer` on 30 inputs: 28 identical texts,
  2 identical failures.
* `node_client`: the reference JS client reads the Rust encodings and every exposed property (source file metadata,
  references, imports, augmentations, external module indicator, each node's kind/pos/end/flags/text/… via
  `forEachChild` incl. NodeArrays) equals its view of the Go encodings and of Go's re-encodings; with
  `TSRS_CODEC_TSC` the reference server prints a Rust-encoded synthesized tree exactly as `tsrs_printer` does.

## Notes on parity

* Positions on the wire are UTF-16 (the encoder converts; the client reads them as-is). Decoded nodes keep
  the wire positions, as in Go (a decoded SourceFile re-encoded converts them again, as Go does).
* Synthesized positions: `0xFFFFFFFF` decodes to `-1` (tsrs positions are `i32`; Go keeps `int` 4294967295).
  Both re-encode to `0xFFFFFFFF` and both printers treat the nodes identically on every tested input; the client
  reads node `pos` with `getInt32` (-1) and NodeList `pos` with `getUint32` (4294967295) either way.
* The reference Go decoder panics on a non-empty raw child list (`JSDocTypeLiteral`, `SyntaxList`) because it
  indexes a zero-length slice; `decode_nodes` returns `DecodeError::GoDecoderPanic` with Go's message at the
  same point (printNode of any file with `@typedef {Object}` + `@property` fails on both).
* `get_index` assigns node ids lazily in sort-comparison order exactly like Go, so the session must keep one
  table per live source file and build it at the same point in the request flow as Go for handles of repeated
  JSDoc nodes to agree under arbitrary prior checker work.

## Known gaps

* The API's project system does not content-map files yet (`tsrs_project` compilerhost.rs; content
  mappers in the project system are phase 2 of notes/contentmappers.md), and the encoder always writes the
  "absent" values for the span map, supplemental/canonical file names, content mapper, virtual file name and
  diagnostic directive fields; those are what the reference encoder writes for an ordinary file. Encoding content-mapper info is not implemented.
* Binder data (header offset 60) is always 0, as in the reference encoder.
* The decoder rejects malformed input that the Go decoder would panic on or misread (out-of-range offsets,
  non-forward sibling links, unknown kinds/enum values, invalid UTF-8 other than WTF-8 surrogates, missing
  required children, the reserved data type); the server turns either into an error response.
* The reference server cannot accept source text with unpaired surrogates (its JSON request decoding rejects
  it), so lone surrogates are covered only through string-literal escapes; `PositionMap` follows Go's
  `DecodeJSStringRune` for them.
* `tsrs_printer` formats kinds in panic messages without Go's `Kind` prefix (`unhandled statement:
  JSImportDeclaration` vs `KindJSImportDeclaration`); the API's `printNode` handler adds the prefix when it turns a
  recovered printer panic into an error (`tsrs_api/src/printing.rs`, `go_kind_text`); other paths, such as
  `formatNodeForInsertion`, do not.
