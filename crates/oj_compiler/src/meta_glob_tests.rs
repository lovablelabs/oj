// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use super::*;
use std::fs;

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    fs::create_dir_all(d.join("content")).unwrap();
    fs::write(d.join("content/a.md"), "# a").unwrap();
    fs::write(d.join("content/b.md"), "# b").unwrap();
    fs::write(d.join("content/index.json"), "{}").unwrap();
    fs::create_dir_all(d.join("pages/home")).unwrap();
    fs::create_dir_all(d.join("pages/about")).unwrap();
    fs::write(d.join("pages/home/page.tsx"), "export default 1").unwrap();
    fs::write(d.join("pages/about/page.tsx"), "export default 2").unwrap();
    dir
}

/// The fixture root, canonicalized (macOS temp dirs sit behind a symlink).
fn root(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().canonicalize().unwrap()
}

fn glob(code: &str, file: &Path, root: &Path) -> String {
    transform_glob(code, file, root, &mut Vec::new()).unwrap()
}

fn re(pattern: &str) -> regex::Regex {
    regex::Regex::new(pattern).unwrap()
}

#[test]
fn code_without_a_glob_is_untouched() {
    let code = "export const x = 1;\nconsole.log('hi');";
    assert_eq!(glob(code, Path::new("/app/x.ts"), Path::new("/app")), code);
}

#[test]
fn a_lazy_glob_becomes_a_map_of_dynamic_imports() {
    let dir = fixture();
    let r = root(&dir);
    let out = glob(
        "const m = import.meta.glob(\"./content/*.md\");",
        &r.join("index.ts"),
        &r,
    );
    assert!(
        re(r#""\./content/a\.md":\s*\(\)\s*=>\s*import\("\./content/a\.md"\)"#).is_match(&out),
        "{out}"
    );
    assert!(out.contains("\"./content/b.md\":"), "{out}");
    assert!(
        !out.contains("index.json") && !out.contains("import.meta.glob"),
        "{out}"
    );
}

#[test]
fn wildcards_omit_hidden_paths_unless_asked_or_exhaustive() {
    let dir = fixture();
    let r = root(&dir);
    fs::write(r.join("content/.hidden.md"), "hidden").unwrap();
    fs::create_dir_all(r.join("content/.draft")).unwrap();
    fs::write(r.join("content/.draft/nested.md"), "draft").unwrap();
    let file = r.join("index.ts");
    let normal = glob(
        "const m = import.meta.glob(\"./content/**/*.md\");",
        &file,
        &r,
    );
    let explicit = glob(
        "const m = import.meta.glob(\"./content/.*.md\");",
        &file,
        &r,
    );
    assert!(normal.contains("./content/a.md"), "{normal}");
    assert!(
        !normal.contains(".hidden.md") && !normal.contains(".draft"),
        "{normal}"
    );
    assert!(explicit.contains("./content/.hidden.md"), "{explicit}");

    fs::create_dir_all(r.join("content/node_modules")).unwrap();
    fs::write(r.join("content/node_modules/dep.md"), "dep").unwrap();
    let exhaustive = glob(
        "const m = import.meta.glob(\"./content/**/*.md\", { exhaustive: true });",
        &file,
        &r,
    );
    assert!(exhaustive.contains(".hidden.md"), "{exhaustive}");
    assert!(exhaustive.contains(".draft/nested.md"), "{exhaustive}");
    assert!(exhaustive.contains("node_modules/dep.md"), "{exhaustive}");
    let plain = glob(
        "const m = import.meta.glob(\"./content/**/*.md\");",
        &file,
        &r,
    );
    assert!(!plain.contains("node_modules"), "{plain}");
}

#[test]
fn type_arguments_are_skipped_however_nested() {
    let dir = fixture();
    let r = root(&dir);
    let file = r.join("index.ts");
    let out = glob(
        "const p = import.meta.glob<string>(\"./pages/*/page.tsx\");",
        &file,
        &r,
    );
    assert!(out.contains("\"./pages/home/page.tsx\":"), "{out}");
    assert!(out.contains("\"./pages/about/page.tsx\":"), "{out}");
    let nested = glob(
        "const m = import.meta.glob<Record<string, unknown>>([\"./content/*.md\"]);",
        &file,
        &r,
    );
    assert!(nested.contains("\"./content/a.md\":"), "{nested}");
    let object = glob(
        "const m = import.meta.glob<{ default: string }>(\"./content/*.md\", { eager: true });",
        &file,
        &r,
    );
    assert!(
        re(r#"^import \* as \w+ from "\./content/a\.md";"#).is_match(&object),
        "{object}"
    );
    let several = glob(
        &[
            "const a = import.meta.glob<{ default: Foo }>(\"./content/*.json\");",
            "const b = import.meta.glob<SidecarModule>(\"./pages/**/*.tsx\");",
            "const c = import.meta.glob<Record<string, unknown>>([\"./content/*.md\"]);",
        ]
        .join("\n"),
        &file,
        &r,
    );
    assert!(!several.contains("import.meta.glob"), "{several}");
    assert!(several.contains("\"./pages/home/page.tsx\":"), "{several}");
    assert!(several.contains("\"./content/a.md\":"), "{several}");
}

#[test]
fn a_pattern_may_climb_above_the_importer() {
    let dir = fixture();
    let r = root(&dir);
    fs::create_dir_all(r.join("app/deep")).unwrap();
    let out = glob(
        "const m = import.meta.glob(\"../../content/*.md\");",
        &r.join("app/deep/index.ts"),
        &r,
    );
    assert!(out.contains("\"../../content/a.md\":"), "{out}");
}

#[test]
fn eager_query_and_import_default_produce_static_imports() {
    let dir = fixture();
    let r = root(&dir);
    let out = glob(
        "const s = import.meta.glob(\"./content/*.md\", { eager: true, query: \"?raw\", import: \"default\" });",
        &r.join("index.ts"),
        &r,
    );
    assert!(out.starts_with("import __oj_glob0_0 from \"./content/a.md?raw\"; import __oj_glob0_1 from \"./content/b.md?raw\";"), "{out}");
    assert!(!out.contains("=> import"), "{out}");
    let lazy = glob(
        "const s = import.meta.glob(\"./content/*.md\", { import: \"default\" });",
        &r.join("index.ts"),
        &r,
    );
    assert!(
        lazy.contains("() => import(\"./content/a.md\").then((m) => m.default)"),
        "{lazy}"
    );
}

#[test]
fn query_objects_serialize_like_url_search_params() {
    let dir = fixture();
    let r = root(&dir);
    let out = glob(
        "const m = import.meta.glob(\"./content/*.md\", { query: { raw: \"\", locale: \"en US\" } });",
        &r.join("index.ts"),
        &r,
    );
    assert!(
        out.contains("import(\"./content/a.md?raw=&locale=en+US\")"),
        "{out}"
    );
    assert!(out.contains("\"./content/a.md\":"), "{out}");
}

#[test]
fn named_imports_select_the_export_in_both_modes() {
    let dir = fixture();
    let r = root(&dir);
    let file = r.join("index.ts");
    let lazy = glob(
        "const m = import.meta.glob(\"./content/*.md\", { import: \"metadata\" });",
        &file,
        &r,
    );
    let eager = glob(
        "const m = import.meta.glob(\"./content/*.md\", { eager: true, import: \"metadata\" });",
        &file,
        &r,
    );
    assert!(
        lazy.contains("import(\"./content/a.md\").then((m) => m[\"metadata\"])"),
        "{lazy}"
    );
    assert!(
        eager.starts_with("import { metadata as __oj_glob0_0 } from \"./content/a.md\";"),
        "{eager}"
    );
    let namespace = glob(
        "const m = import.meta.glob(\"./content/*.md\", { import: \"*\" });",
        &file,
        &r,
    );
    assert!(
        namespace.contains("() => import(\"./content/a.md\")}")
            || namespace.contains("() => import(\"./content/a.md\"),"),
        "{namespace}"
    );
}

#[test]
fn negated_patterns_exclude_matches() {
    let dir = fixture();
    let r = root(&dir);
    let out = glob(
        "const m = import.meta.glob([\"./content/*.md\", \"!./content/b.md\"]);",
        &r.join("index.ts"),
        &r,
    );
    assert!(out.contains("\"./content/a.md\":"), "{out}");
    assert!(!out.contains("\"./content/b.md\":"), "{out}");
}

// Vite's dialect (picomatch via tinyglobby): `**` spans whole parts only,
// classes and braces expand, and the importer's `[id]` directory is matched
// literally (Vite's globSafePath).
#[test]
fn globs_expand_in_picomatchs_dialect() {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path().canonicalize().unwrap();
    for f in [
        "x/a/m.json",
        "x/b/m.json",
        "x/c/m.json",
        "c/foox.json",
        "c/d/x.json",
        "r/[id]/one.ts",
        "r/[id]/two.ts",
    ] {
        fs::create_dir_all(r.join(f).parent().unwrap()).unwrap();
        fs::write(r.join(f), "{}").unwrap();
    }
    let expand = |pattern: &str, importer: &str| {
        glob(
            &format!("const m = import.meta.glob({});", json(pattern)),
            &r.join(importer),
            &r,
        )
    };
    for pattern in ["./x/[ab]/*.json", "./x/{a,b}/*.json", "./x/[!c]/*.json"] {
        let out = expand(pattern, "index.ts");
        assert!(out.contains("\"./x/a/m.json\""), "{pattern}: {out}");
        assert!(out.contains("\"./x/b/m.json\""), "{pattern}: {out}");
        assert!(!out.contains("/c/m.json"), "{pattern}: {out}");
    }
    let deep = expand("./c/**/x.json", "index.ts");
    assert!(
        deep.contains("\"./c/d/x.json\"") && !deep.contains("foox"),
        "{deep}"
    );
    assert!(expand("./*.ts", "r/[id]/one.ts").contains("\"./two.ts\""));
}

// What the dev rebundle gate reads: each call's patterns as Vite resolves
// them (prefixes glob-escaped, negatives kept) and its options.
#[test]
fn scopes_collect_each_calls_resolved_patterns_and_options() {
    let mut scopes = Vec::new();
    transform_glob(
        "const m = import.meta.glob(['./*.ts', '!./skip.ts'], { caseSensitive: false, exhaustive: true });",
        Path::new("/app/r/[id]/one.ts"),
        Path::new("/app"),
        &mut scopes,
    )
    .unwrap();
    assert_eq!(
        scopes,
        [GlobCall {
            patterns: vec![
                "/app/r/\\[id\\]/*.ts".to_string(),
                "!/app/r/\\[id\\]/skip.ts".to_string()
            ],
            exhaustive: true,
            case_sensitive: false,
        }]
    );
}

#[test]
fn an_empty_glob_yields_an_empty_map() {
    let dir = fixture();
    let r = root(&dir);
    let out = glob(
        "const m = import.meta.glob(\"./nope/*.md\");",
        &r.join("index.ts"),
        &r,
    );
    assert_eq!(out, "const m = {};");
}

#[test]
fn the_rewrite_adds_no_lines() {
    let dir = fixture();
    let r = root(&dir);
    let code = [
        "// header",
        "const eager = import.meta.glob(",
        "  \"./content/*.md\",",
        "  { eager: true },",
        ") as Record<string, unknown>;",
        "const lazy = import.meta.glob(\"./pages/**/*.tsx\");",
        "export const marker = 1;",
    ]
    .join("\n");
    let out = glob(&code, &r.join("index.ts"), &r);
    let lines: Vec<&str> = out.split('\n').collect();
    assert_eq!(lines.len(), code.split('\n').count(), "{out}");
    assert_eq!(
        lines[0],
        "import * as __oj_glob0_0 from \"./content/a.md\"; import * as __oj_glob0_1 from \"./content/b.md\"; // header"
    );
    assert_eq!(
        lines[1],
        "const eager = {\"./content/a.md\": __oj_glob0_0, \"./content/b.md\": __oj_glob0_1"
    );
    assert_eq!(lines[4], "} as Record<string, unknown>;");
    assert!(
        re(r"^const lazy = \{.*\};$").is_match(lines[5]),
        "{}",
        lines[5]
    );
    assert_eq!(lines[6], "export const marker = 1;");
}

// Vite's `base` option: patterns resolve against the base, root-relative
// when it starts with `/`, and the keys are relative to that base; import
// specifiers stay relative to the importer.
#[test]
fn the_base_option_resolves_patterns_and_keys_against_the_base() {
    let dir = fixture();
    let r = root(&dir);
    fs::create_dir_all(r.join("app/deep")).unwrap();
    let importer = r.join("app/deep/index.ts");
    for base in ["../../content", "/content"] {
        let out = glob(
            &format!("const m = import.meta.glob(\"./*.md\", {{ base: \"{base}\" }});"),
            &importer,
            &r,
        );
        assert!(
            out.contains("\"./a.md\": () => import(\"../../content/a.md\")"),
            "{base}: {out}"
        );
        assert!(out.contains("\"./b.md\":"), "{base}: {out}");
    }
    let err = transform_glob(
        "const m = import.meta.glob(\"./*.md\", { base: \"content\" });",
        &importer,
        &r,
        &mut Vec::new(),
    )
    .unwrap_err();
    assert!(err.0.contains("Option \"base\" must start with"), "{err}");
}

#[test]
fn root_relative_patterns_resolve_from_the_root_with_slash_keys() {
    let dir = fixture();
    let r = root(&dir);
    fs::create_dir_all(r.join("app")).unwrap();
    let out = glob(
        "const m = import.meta.glob(\"/pages/**/page.tsx\");",
        &r.join("app/index.ts"),
        &r,
    );
    assert!(
        out.contains("\"/pages/home/page.tsx\": () => import(\"../pages/home/page.tsx\")"),
        "{out}"
    );
    assert!(out.contains("\"/pages/about/page.tsx\":"), "{out}");
}

// The deprecated `as` option is `query` by another name; `as: 'raw' | 'url'`
// forces the default import, and mixing `as` with `query` is rejected.
#[test]
fn the_as_option_maps_to_a_query() {
    let dir = fixture();
    let r = root(&dir);
    let file = r.join("index.ts");
    let raw = glob(
        "const m = import.meta.glob(\"./content/*.md\", { as: \"raw\", eager: true });",
        &file,
        &r,
    );
    assert!(
        raw.starts_with("import __oj_glob0_0 from \"./content/a.md?raw\";"),
        "{raw}"
    );
    let url = glob(
        "const m = import.meta.glob(\"./content/*.md\", { as: \"url\" });",
        &file,
        &r,
    );
    assert!(
        url.contains("import(\"./content/a.md?url\").then((m) => m.default)"),
        "{url}"
    );
    let bare = glob(
        "const m = import.meta.glob(\"./content/*.md\", { query: \"raw\" });",
        &file,
        &r,
    );
    assert!(bare.contains("import(\"./content/a.md?raw\")"), "{bare}");
    let both = transform_glob(
        "const m = import.meta.glob(\"./content/*.md\", { as: \"raw\", query: \"?x\" });",
        &file,
        &r,
        &mut Vec::new(),
    )
    .unwrap_err();
    assert!(
        both.0
            .contains("\"as\" and \"query\" cannot be used together"),
        "{both}"
    );
    let named = transform_glob(
        "const m = import.meta.glob(\"./content/*.md\", { as: \"raw\", import: \"named\" });",
        &file,
        &r,
        &mut Vec::new(),
    )
    .unwrap_err();
    assert!(
        named.0.contains("can only be \"default\" or \"*\""),
        "{named}"
    );
}

#[test]
fn case_insensitive_matching_and_the_importer_itself_is_skipped() {
    let dir = fixture();
    let r = root(&dir);
    fs::write(r.join("content/Upper.MD"), "# u").unwrap();
    let file = r.join("index.ts");
    let strict = glob("const m = import.meta.glob(\"./content/*.md\");", &file, &r);
    assert!(!strict.contains("Upper.MD"), "{strict}");
    let loose = glob(
        "const m = import.meta.glob(\"./content/*.md\", { caseSensitive: false });",
        &file,
        &r,
    );
    assert!(loose.contains("\"./content/Upper.MD\":"), "{loose}");
    assert!(loose.contains("\"./content/a.md\":"), "{loose}");
    fs::write(r.join("pages/index.ts"), "").unwrap();
    let own = glob(
        "const m = import.meta.glob(\"./**/*.ts\");",
        &r.join("pages/index.ts"),
        &r,
    );
    assert!(!own.contains("\"./index.ts\""), "{own}");
}

#[test]
fn a_call_with_non_static_arguments_is_left_as_written() {
    let code = "const m = import.meta.glob(pattern);\nconst n = import.meta.globEager(\"./x\");";
    assert_eq!(glob(code, Path::new("/app/x.ts"), Path::new("/app")), code);
}

// Vite's vite:worker-import-meta-url and vite:asset-import-meta-url on a
// client module: the URL points at the file's worker/asset import.
fn url_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    fs::create_dir_all(d.join("img")).unwrap();
    fs::write(d.join("draw-worker.ts"), "self.onmessage = () => {};").unwrap();
    fs::write(d.join("img/a.png"), "").unwrap();
    fs::write(d.join("img/b.png"), "").unwrap();
    dir
}

/// What a bundler resolves a site's file to here: relative files only.
fn rewrite(code: &str, id: &Path, root: &Path, self_entry: Option<&Path>) -> String {
    let sites = scan_import_meta_urls(code, &id.to_string_lossy()).unwrap();
    let targets: Vec<MetaUrlTarget> = sites
        .iter()
        .map(|s| {
            let Some(file) = s.kind.file() else {
                return MetaUrlTarget::Unresolved;
            };
            if has_scheme(file) {
                return MetaUrlTarget::Unresolved;
            }
            let abs = if let Some(rest) = file.strip_prefix('/') {
                root.join(rest)
            } else {
                id.parent().unwrap().join(file.trim_start_matches("./"))
            };
            if !abs.is_file() {
                return MetaUrlTarget::Unresolved;
            }
            if Some(abs.as_path()) == self_entry {
                MetaUrlTarget::SelfWorker
            } else {
                MetaUrlTarget::File(abs)
            }
        })
        .collect();
    apply_import_meta_urls(code, &sites, &targets)
}

#[test]
fn a_worker_url_imports_the_workers_worker_url() {
    let dir = url_fixture();
    let r = root(&dir);
    let code = "const w = new Worker(new URL(\"./draw-worker.ts\", import.meta.url), { type: \"module\" });";
    let out = rewrite(code, &r.join("game.ts"), &r, None);
    let spec = json(&format!(
        "{}?worker&url",
        r.join("draw-worker.ts").display()
    ));
    assert!(
        out.starts_with(&format!("import __oj_worker_url0 from {spec};")),
        "{out}"
    );
    assert!(
        out.contains("new Worker(new URL(/* @vite-ignore */ __oj_worker_url0, '' + import.meta.url), { type: \"module\" })"),
        "{out}"
    );
    assert!(!out.contains("\"./draw-worker.ts\""), "{out}");
}

#[test]
fn a_shared_worker_url_keeps_its_query() {
    let dir = url_fixture();
    let r = root(&dir);
    let out = rewrite(
        "new SharedWorker(new URL('./draw-worker.ts?x=1', import.meta.url))",
        &r.join("game.ts"),
        &r,
        None,
    );
    let spec = json(&format!(
        "{}?sharedworker&url&x=1",
        r.join("draw-worker.ts").display()
    ));
    assert!(out.contains(&spec), "{out}");
}

#[test]
fn an_asset_url_imports_the_assets_url() {
    let dir = url_fixture();
    let r = root(&dir);
    let out = rewrite(
        "export const a = new URL(\"./img/a.png\", import.meta.url).href;",
        &r.join("game.ts"),
        &r,
        None,
    );
    let spec = json(&format!("{}?url", r.join("img/a.png").display()));
    assert!(
        out.starts_with(&format!("import __oj_asset_url0 from {spec};")),
        "{out}"
    );
    assert!(
        out.contains("new URL(__oj_asset_url0, '' + import.meta.url).href"),
        "{out}"
    );
}

#[test]
fn a_template_asset_url_becomes_the_eager_url_glob_lookup() {
    let dir = url_fixture();
    let r = root(&dir);
    let id = r.join("game.ts");
    let code = "const n = 'a';\nexport const u = new URL(`./img/${n}.png`, import.meta.url);";
    let out = rewrite(code, &id, &r, None);
    assert!(
        out.contains("new URL((import.meta.glob(\"./img/*.png\", {\"eager\":true,\"import\":\"default\",\"query\":\"?url\"}))[`./img/${n}.png`], import.meta.url)"),
        "{out}"
    );
    let expanded = glob(&out, &id, &r);
    assert!(
        expanded.contains("import __oj_glob0_0 from \"./img/a.png?url\";"),
        "{expanded}"
    );
    assert!(
        expanded.contains("\"./img/b.png\": __oj_glob0_1"),
        "{expanded}"
    );
}

#[test]
fn comments_strings_data_urls_and_ignored_sites_stay_as_written() {
    let dir = url_fixture();
    let r = root(&dir);
    let code = [
        "// new Worker(new URL(\"./draw-worker.ts\", import.meta.url))",
        "const s = \"new URL('./img/a.png', import.meta.url)\";",
        "const d = new URL(\"data:text/plain,hi\", import.meta.url);",
        "const i = new URL(/* @vite-ignore */ \"./img/a.png\", import.meta.url);",
        "const m = new URL(\"./img/missing.png\", import.meta.url);",
        "const w = new Worker(new URL(\"./missing-worker.ts\", import.meta.url));",
        "",
    ]
    .join("\n");
    assert_eq!(rewrite(&code, &r.join("game.ts"), &r, None), code);
    let sites = scan_import_meta_urls(&code, "game.ts").unwrap();
    let files: Vec<_> = sites.iter().filter_map(|s| s.kind.file()).collect();
    assert_eq!(files, ["./missing-worker.ts", "./img/missing.png"]);
}

#[test]
fn a_dynamic_template_worker_url_is_an_error() {
    let err = scan_import_meta_urls(
        "new Worker(new URL(`./${n}.ts`, import.meta.url))",
        "/app/x.ts",
    )
    .unwrap_err();
    assert!(
        err.0.contains("not supported in dynamic template string"),
        "{err}"
    );
}

#[test]
fn the_url_rewrite_keeps_every_line() {
    let dir = url_fixture();
    let r = root(&dir);
    let code = "a();\nconst w = new Worker(\n  new URL(\n    \"./draw-worker.ts\",\n    import.meta.url,\n  ),\n);\nb();";
    let out = rewrite(code, &r.join("game.ts"), &r, None);
    assert_eq!(out.split('\n').count(), code.split('\n').count(), "{out}");
    assert_eq!(out.rsplit('\n').next(), Some("b();"));
}

#[test]
fn an_asset_query_joins_its_import_and_a_hash_stays_on_the_url() {
    let dir = url_fixture();
    let r = root(&dir);
    let code = "new URL(\"./img/a.png?inline\", import.meta.url); new URL(\"./img/b.png#top\", import.meta.url);";
    let out = rewrite(code, &r.join("game.ts"), &r, None);
    assert!(
        out.contains(&json(&format!(
            "{}?url&inline",
            r.join("img/a.png").display()
        ))),
        "{out}"
    );
    assert!(
        out.contains(&json(&format!("{}?url", r.join("img/b.png").display()))),
        "{out}"
    );
    assert!(
        out.contains("new URL(__oj_asset_url1 + \"#top\", '' + import.meta.url)"),
        "{out}"
    );
}

#[test]
fn literals_strip_like_strip_literal() {
    assert_eq!(
        strip_literals("a(`x${b('c')}y`) // d"),
        "a(` ${b(' ')} `)     "
    );
    assert_eq!(
        strip_literals("x = /'[/]\\//g; y = a / b / c;"),
        "x = /      /g; y = a / b / c;"
    );
    // Multi-byte text blanks to as many spaces as it has bytes.
    let s = strip_literals("const s = \"ação\"; f()");
    assert_eq!(s.len(), "const s = \"ação\"; f()".len());
    assert!(s.ends_with("\"; f()"), "{s}");

    let dir = url_fixture();
    let r = root(&dir);
    let code = [
        "const q = /'/;",
        "const html = `<p>${new URL(\"./img/a.png\", import.meta.url)}</p>`;",
        "const w = new Worker(new URL(\"./draw-worker.ts\", import.meta.url));",
        "",
    ]
    .join("\n");
    let out = rewrite(&code, &r.join("game.ts"), &r, None);
    assert!(
        out.contains("${new URL(__oj_asset_url1, '' + import.meta.url)}"),
        "{out}"
    );
    assert!(
        out.contains("new Worker(new URL(/* @vite-ignore */ __oj_worker_url0"),
        "{out}"
    );
}

#[test]
fn a_worker_starting_itself_gets_self_location_href() {
    let dir = url_fixture();
    let r = root(&dir);
    let id = r.join("draw-worker.ts");
    let out = rewrite(
        "new Worker(new URL(\"./draw-worker.ts\", import.meta.url), { type: \"module\" });",
        &id,
        &r,
        Some(&id),
    );
    assert_eq!(out, "new Worker(self.location.href, { type: \"module\" });");
}

#[test]
fn schemes_are_no_files() {
    assert!(has_scheme("https://x.test/a.png"));
    assert!(has_scheme("blob:abc"));
    assert!(!has_scheme("./a.png"));
    assert!(!has_scheme("/a.png"));
    assert!(!has_scheme("img/a.png"));
}

// Vite finds the calls in the AST: one spelled inside a string or a comment
// is text, never expanded (a string's quotes would break, a comment's eager
// glob would hoist real imports).
#[test]
fn a_glob_inside_a_string_or_comment_is_left_alone() {
    let dir = fixture();
    let r = root(&dir);
    let code = concat!(
        "const doc = \"use import.meta.glob('./content/*.md') here\";\n",
        "// const old = import.meta.glob('./content/*.md', { eager: true });\n",
        "/* import.meta.glob('./pages/**/page.tsx') */\n",
        "const real = import.meta.glob('./content/*.md');\n",
    );
    let out = glob(code, &r.join("main.ts"), &r);
    assert!(
        out.starts_with("const doc = \"use import.meta.glob('./content/*.md') here\";\n"),
        "{out}"
    );
    assert!(
        out.contains("// const old = import.meta.glob('./content/*.md', { eager: true });"),
        "{out}"
    );
    assert!(
        out.contains("/* import.meta.glob('./pages/**/page.tsx') */"),
        "{out}"
    );
    assert!(
        !out.contains("import __oj_glob"),
        "a comment's eager glob hoists nothing:\n{out}"
    );
    assert!(
        out.contains("\"./content/a.md\": () => import("),
        "the real call expands:\n{out}"
    );
    assert!(!out.contains("page.tsx\": "), "{out}");
    assert_eq!(out.split('\n').count(), code.split('\n').count(), "{out}");
}
