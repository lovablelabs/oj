// Go `internal/spanmap`, ported in `tsrs_spanmap`. The language server does not create span maps yet (content mappers
// in the language server are phase 2 of notes/contentmappers.md): `Script::span_map` of a `SourceFile` is `None`.

pub use tsrs_spanmap::*;
