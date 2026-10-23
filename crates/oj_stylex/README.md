# oj_stylex

Rust compiler for StyleX, targeting `@stylexjs/babel-plugin` 0.19.0 behaviour.

Source transforms, OXC AST transforms and deterministic stylesheet assembly
are provided by the `api` and `assemble` modules. AST integration requires the
same OXC version as the compiler's public types. The `cli` module is the
command set behind `oj stylex` and the standalone `fru` binary; the resident
`serve` protocol is described in [`docs/serve-protocol.md`](docs/serve-protocol.md).

The conformance corpus and test pins stay in the repository and are left out
of the published crate. Upstream-derived fixtures retain their StyleX MIT
licence in `conformance/LICENSE-StyleX`.
