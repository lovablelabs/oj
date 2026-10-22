// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;
use std::path::Path;

use super::{build_app, scratch, write_files};
use crate::build::ChainHook;

const MAIN: &str = "const a = 1;\nconst b = 2;\nconsole.log(\"MARK\", a + b);\n";

/// An app whose plugin pushes `main.js` (or the chunk) down two lines in
/// `hook` and returns the map that undoes it.
fn shifting_app(root: &Path, hook: &str) {
    let plugins = format!(
        r#"const shift = (code) => ({{
  code: "\n\n" + code,
  map: {{ version: 3, sources: ["in.js"], names: [], mappings: ";;AAAA" + ";AACA".repeat(code.split("\n").length - 1) }},
}});
export default [{{
  name: "shift",
  {hook},
}}];
"#
    );
    write_files(
        root,
        &[
            (
                "package.json",
                r#"{"name":"smap","version":"1.0.0","type":"module"}"#,
            ),
            (
                "oj.config.json",
                r#"{"build":{"sourcemap":true,"minify":false}}"#,
            ),
            ("src/main.js", MAIN),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
            ("oj.plugins.mjs", &plugins),
        ],
    );
}

/// Where the built `"MARK"` maps back to: (source, line).
fn mark_origin(root: &Path) -> Option<(String, u32)> {
    let assets = root.join("dist/assets");
    let js = fs::read_dir(&assets)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| {
            p.extension().is_some_and(|e| e == "js")
                && fs::read_to_string(p).unwrap().contains("\"MARK\"")
        })
        .expect("the entry chunk");
    let code = fs::read_to_string(&js).unwrap();
    let (line, text) = code
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("\"MARK\""))
        .unwrap();
    let col = text.find("\"MARK\"").unwrap() as u32;
    let json = fs::read_to_string(js.with_extension("js.map")).unwrap();
    let map = oxc_sourcemap::SourceMap::from_json_string(&json).unwrap();
    let lut = map.generate_lookup_table();
    let t = map.lookup_token(&lut, line as u32, col)?;
    if t.get_dst_line() != line as u32 {
        return None;
    }
    let source = map.get_source(t.get_source_id()?)?.to_string();
    Some((source, t.get_src_line()))
}

/// Rolldown takes one map per hook, so oj collapses the host's chain into
/// it. Without one, rolldown counts the map as missing and the module lost
/// every mapping.
#[tokio::test]
async fn a_plugin_transform_map_traces_the_bundle_to_the_original_source() {
    let root = scratch("smap-transform");
    shifting_app(
        &root,
        r#"transform(code, id) { return id.endsWith("/src/main.js") ? shift(code) : null; }"#,
    );
    build_app(&root).await.expect("build");
    let (source, line) = mark_origin(&root).expect("MARK is mapped");
    assert!(source.ends_with("src/main.js"), "{source}");
    assert_eq!(line, 2, "MARK is on main.js line 3");
    fs::remove_dir_all(&root).unwrap();
}

/// A renderChunk map joins the chunk's chain too; dropped, every position
/// after the plugin's edit was off by its shift.
#[tokio::test]
async fn a_plugin_render_chunk_map_keeps_the_chunk_map_aligned() {
    let root = scratch("smap-render-chunk");
    shifting_app(&root, "renderChunk(code) { return shift(code); }");
    build_app(&root).await.expect("build");
    let (source, line) = mark_origin(&root).expect("MARK is mapped");
    assert!(source.ends_with("src/main.js"), "{source}");
    assert_eq!(line, 2, "MARK is on main.js line 3");
    fs::remove_dir_all(&root).unwrap();
}

/// Two plugins that each push the code down a line collapse into one map
/// two lines deep; a plugin that left no map breaks it.
#[test]
fn the_host_chain_collapses_in_order_and_a_missing_map_breaks_it() {
    use rolldown_plugin::HookTransformOutputMap;
    let down_one = r#"{"version":3,"sources":["a.js"],"names":[],"mappings":";AAAA;AACA"}"#;
    let mut t = oj_server::plugins::HostTransform {
        maps: vec![down_one.to_string(), down_one.to_string()],
        ..Default::default()
    };
    let HookTransformOutputMap::Sourcemap(map) =
        crate::build::host_chain_map(true, &t, ChainHook::Transform, None)
    else {
        panic!("two maps collapse into one");
    };
    let lut = map.generate_lookup_table();
    let tok = map.lookup_token(&lut, 2, 0).expect("line 3 is mapped");
    assert_eq!((tok.get_dst_line(), tok.get_src_line()), (2, 0));

    t.map_broken = Some("raw".into());
    assert!(matches!(
        crate::build::host_chain_map(true, &t, ChainHook::Transform, None),
        HookTransformOutputMap::Omitted
    ));
    t.map_broken = None;
    t.maps.clear();
    assert!(matches!(
        crate::build::host_chain_map(true, &t, ChainHook::Transform, None),
        HookTransformOutputMap::Null
    ));
}

/// A map the plugin left without `version`, `sources` or `names` reads as
/// rolldown's JS side reads it.
#[test]
fn a_map_without_version_or_sources_still_joins_the_chain() {
    use rolldown_plugin::HookTransformOutputMap;
    let t = oj_server::plugins::HostTransform {
        maps: vec![r#"{"mappings":";AAAA"}"#.to_string()],
        map_plugins: vec!["terse".into()],
        ..Default::default()
    };
    let HookTransformOutputMap::Sourcemap(map) =
        crate::build::host_chain_map(true, &t, ChainHook::Transform, None)
    else {
        panic!("the terse map reads");
    };
    let lut = map.generate_lookup_table();
    let tok = map.lookup_token(&lut, 1, 0).expect("line 2 is mapped");
    assert_eq!(tok.get_src_line(), 0);
}

/// A map oj cannot read breaks the chain and names the plugin that sent it;
/// each warning prints once per build, and the next build warns again.
#[test]
fn an_unreadable_map_breaks_the_chain_and_warns_once_per_build() {
    use rolldown_plugin::HookTransformOutputMap;
    let good = r#"{"version":3,"sources":["a.js"],"names":[],"mappings":"AAAA"}"#;
    let t = oj_server::plugins::HostTransform {
        maps: vec![good.to_string(), r#"{"mappings":[[0,0,0,0]]}"#.to_string()],
        map_plugins: vec!["fine".into(), "decoded".into()],
        ..Default::default()
    };
    let build = crate::build::MapWarnings::default();
    for _ in 0..2 {
        assert!(matches!(
            crate::build::host_chain_map(true, &t, ChainHook::Transform, Some(&build)),
            HookTransformOutputMap::Omitted
        ));
    }
    let printed = build.printed();
    assert_eq!(printed.len(), 1, "{printed:?}");
    assert!(printed[0].contains("(decoded)"), "{printed:?}");

    let next = crate::build::MapWarnings::default();
    crate::build::host_chain_map(true, &t, ChainHook::Transform, Some(&next));
    assert_eq!(next.printed().len(), 1, "a new build warns again");

    let off = crate::build::MapWarnings::default();
    crate::build::host_chain_map(false, &t, ChainHook::Transform, Some(&off));
    assert!(off.printed().is_empty(), "no sourcemap, no warning");
}

/// A renderChunk step without a map skips only its own map, as in rolldown:
/// a footer plugin that returns a string leaves the next plugin's map in
/// the chunk's chain.
#[tokio::test]
async fn a_render_chunk_step_without_a_map_keeps_the_others() {
    let root = scratch("smap-render-chunk-footer");
    shifting_app(&root, "renderChunk(code) { return shift(code); }");
    let plugins = fs::read_to_string(root.join("oj.plugins.mjs")).unwrap().replace(
        "export default [{",
        "export default [{ name: \"footer\", renderChunk(code) { return code + \"\\n// footer\\n\"; } }, {",
    );
    fs::write(root.join("oj.plugins.mjs"), plugins).unwrap();
    build_app(&root).await.expect("build");
    let (source, line) = mark_origin(&root).expect("MARK is mapped");
    assert!(source.ends_with("src/main.js"), "{source}");
    assert_eq!(line, 2, "MARK is on main.js line 3");
    fs::remove_dir_all(&root).unwrap();
}

/// In a renderChunk chain, a step without a map or with one oj cannot read
/// drops out on its own and the other maps still collapse.
#[test]
fn a_render_chunk_chain_skips_only_its_unmapped_steps() {
    use rolldown_plugin::HookTransformOutputMap;
    let down_one = r#"{"version":3,"sources":["a.js"],"names":[],"mappings":";AAAA;AACA"}"#;
    let t = oj_server::plugins::HostTransform {
        maps: vec![down_one.to_string(), "not a map".to_string()],
        map_plugins: vec!["shift".into(), "garbled".into()],
        map_broken: Some("footer".into()),
        ..Default::default()
    };
    let warnings = crate::build::MapWarnings::default();
    let HookTransformOutputMap::Sourcemap(map) =
        crate::build::host_chain_map(true, &t, ChainHook::RenderChunk, Some(&warnings))
    else {
        panic!("the readable map survives");
    };
    let lut = map.generate_lookup_table();
    assert_eq!(map.lookup_token(&lut, 1, 0).unwrap().get_src_line(), 0);
    let mut printed = warnings.printed();
    printed.sort();
    assert_eq!(printed.len(), 2, "{printed:?}");
    assert!(
        printed[0].contains("(footer)") && printed[1].contains("(garbled)"),
        "{printed:?}"
    );
}
