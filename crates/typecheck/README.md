# Type checker crates

`oj_typecheck` is oj's TypeScript type checker (`oj typecheck`, and
`/@oj/typecheck` under `oj dev --typecheck`). It runs on
[tsrs](https://github.com/oxc-project/tsrs), the Rust port of the
TypeScript 7 compiler, whose crates are vendored here as `oj_tsrs_*`.

Vendored: tsrs 0.20.1 (`77d3cddb44540734abc33a42ccca4d3f33bee527`), TypeScript 7.1.0-beta.
Licensed Apache-2.0; each crate carries tsrs's `LICENSE` and `NOTICE`.

Do not edit the `oj_tsrs_*` crates by hand. To move to another tsrs
release, check it out and rerun the script, which also rewrites the
`tsrs:` blocks of the root `Cargo.toml`:

```sh
git clone --depth 1 --branch v<version> https://github.com/oxc-project/tsrs /tmp/tsrs
tools/vendor-tsrs.py /tmp/tsrs
```

The script's docstring lists the patches it applies.

- `oj_tsrs_api`
- `oj_tsrs_api_codec`
- `oj_tsrs_api_transport`
- `oj_tsrs_ast`
- `oj_tsrs_astnav`
- `oj_tsrs_binder`
- `oj_tsrs_checker`
- `oj_tsrs_compiler`
- `oj_tsrs_contentmapper`
- `oj_tsrs_core`
- `oj_tsrs_declarations`
- `oj_tsrs_diagnostics`
- `oj_tsrs_execute`
- `oj_tsrs_fswatch`
- `oj_tsrs_incremental`
- `oj_tsrs_ipc`
- `oj_tsrs_ls`
- `oj_tsrs_lsproto`
- `oj_tsrs_module`
- `oj_tsrs_modulespecifiers`
- `oj_tsrs_parser`
- `oj_tsrs_printer`
- `oj_tsrs_project`
- `oj_tsrs_projectutil`
- `oj_tsrs_pseudochecker`
- `oj_tsrs_scanner`
- `oj_tsrs_sourcemap`
- `oj_tsrs_spanmap`
- `oj_tsrs_transformers`
- `oj_tsrs_tsoptions`
- `oj_tsrs_vfs`
