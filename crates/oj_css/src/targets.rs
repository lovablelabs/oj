#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;

use lightningcss::targets::{Browsers, Targets};

/// Vite 8's `baseline-widely-available` (the `build.cssTarget` default).
const BASELINE: &[&str] = &[
    "chrome111",
    "edge111",
    "firefox114",
    "safari16.4",
    "ios16.4",
];

/// Vite's legacy `modules` preset.
const MODULES: &[&str] = &["chrome87", "edge88", "firefox78", "safari14"];

/// The browsers an `es6` / `es<year>` target stands for, from Vite's `esMap`
/// (css.ts); `None` for any other name.
fn es_browsers(entry: &str) -> Option<Vec<&'static str>> {
    let browsers = match entry.strip_prefix("es")? {
        "6" | "2015" => "chrome49 edge13 safari10 ios10 firefox44 opera36",
        "2016" => "chrome50 edge13 safari10 ios10 firefox43 opera37",
        "2017" => "chrome58 edge15 safari11 ios11 firefox52 opera45",
        "2018" => "chrome63 edge79 safari12 ios12 firefox58 opera50",
        "2019" => "chrome73 edge79 safari12.1 ios12.1 firefox64 opera60",
        "2020" => "chrome80 edge80 safari14.1 ios14.5 firefox80 opera67",
        "2021" => "chrome85 edge85 safari14.1 ios14.5 firefox80 opera71",
        "2022" => "chrome94 edge94 safari16.4 ios16.4 firefox93 opera80",
        "2023" => "chrome110 edge110 safari16.4 ios16.4 firefox146 opera96",
        "2024" => "chrome119 edge119 safari17.4 ios17.4 firefox145 opera105",
        "2025" => "chrome136 edge136 safari26.0 ios26.0 firefox138 opera121",
        _ => return None,
    };
    Some(browsers.split_whitespace().collect())
}

/// esbuild-style targets as lightningcss browsers, like Vite's
/// `convertTargets`: `es<year>` expands to Vite's browsers for that year, the
/// lowest version per browser wins, other non-browser names (`esnext`,
/// `node18`) are skipped, and no browser at all means the baseline.
pub fn browser_targets(list: &[String]) -> Targets {
    let expanded = list
        .iter()
        .flat_map(|t| es_browsers(t).unwrap_or_else(|| vec![t.as_str()]));
    let browsers = parse_browsers(expanded).or_else(|| {
        let preset = if list.iter().any(|t| t == "modules") {
            MODULES
        } else {
            BASELINE
        };
        parse_browsers(preset.iter().copied())
    });
    Targets::from(browsers.expect("presets name browsers"))
}

fn parse_browsers<'a>(list: impl Iterator<Item = &'a str>) -> Option<Browsers> {
    let mut b = Browsers::default();
    let mut any = false;
    for entry in list {
        let (name, version) = entry.split_at(
            entry
                .find(|c: char| c.is_ascii_digit())
                .unwrap_or(entry.len()),
        );
        let Some(v) = parse_version(version) else {
            continue;
        };
        let slot = match name.to_ascii_lowercase().as_str() {
            "chrome" => &mut b.chrome,
            "edge" => &mut b.edge,
            "firefox" => &mut b.firefox,
            "ie" => &mut b.ie,
            "ios" | "ios_saf" => &mut b.ios_saf,
            "opera" => &mut b.opera,
            "safari" => &mut b.safari,
            "android" => &mut b.android,
            "samsung" => &mut b.samsung,
            _ => continue,
        };
        *slot = Some(slot.map_or(v, |cur| cur.min(v)));
        any = true;
    }
    any.then_some(b)
}

#[cfg(not(target_arch = "wasm32"))]
/// The targets `browserslist()` gives a plugin running in the app: the
/// `browserslist` config found from `root` (package.json, `.browserslistrc`)
/// under `env`, else browserslist's `defaults`.
pub fn browserslist_targets(root: &Path, env: &str) -> Result<Targets, String> {
    let opts = browserslist::Opts {
        path: Some(root.to_string_lossy().into_owned()),
        env: Some(env.to_string()),
        ..Default::default()
    };
    let distribs = browserslist::execute(&opts).map_err(|e| format!("browserslist: {e}"))?;
    let mut b = Browsers::default();
    for distrib in distribs {
        // A range (`ios_saf 16.4-16.5`) counts as its low end.
        let low = distrib.version().split('-').next().unwrap_or_default();
        let Some(v) = parse_version(low) else {
            continue;
        };
        let slot = match distrib.name() {
            "android" => &mut b.android,
            "chrome" | "and_chr" => &mut b.chrome,
            "edge" => &mut b.edge,
            "firefox" | "and_ff" => &mut b.firefox,
            "ie" => &mut b.ie,
            "ios_saf" => &mut b.ios_saf,
            "opera" | "op_mob" => &mut b.opera,
            "safari" => &mut b.safari,
            "samsung" => &mut b.samsung,
            _ => continue,
        };
        *slot = Some(slot.map_or(v, |cur| cur.min(v)));
    }
    Ok(Targets::from(b))
}

/// A lightningcss `targets` object (`{ chrome: 95 << 16, ... }`), the shape
/// its JS API takes; unknown browsers and non-numbers are skipped.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn browsers_from_json(targets: &serde_json::Value) -> Option<Browsers> {
    let mut b = Browsers::default();
    let mut any = false;
    for (name, version) in targets.as_object()? {
        let Some(v) = version.as_u64().and_then(|v| u32::try_from(v).ok()) else {
            continue;
        };
        let slot = match name.as_str() {
            "android" => &mut b.android,
            "chrome" => &mut b.chrome,
            "edge" => &mut b.edge,
            "firefox" => &mut b.firefox,
            "ie" => &mut b.ie,
            "ios_saf" => &mut b.ios_saf,
            "opera" => &mut b.opera,
            "safari" => &mut b.safari,
            "samsung" => &mut b.samsung,
            _ => continue,
        };
        *slot = Some(v);
        any = true;
    }
    any.then_some(b)
}

/// `16.4` as lightningcss' `major << 16 | minor << 8`.
fn parse_version(version: &str) -> Option<u32> {
    let mut parts = version.split('.').map(|v| v.parse::<u32>().ok());
    let major = parts.next()??;
    let minor = parts.next().flatten().unwrap_or(0);
    Some((major << 16) | (minor << 8))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn stylex_pass_uses_the_apps_browserslist() {
        let dir = tempfile::tempdir().unwrap();
        let css = ".a{user-select:none}";
        // No config: browserslist `defaults`, which still lists Safari.
        let none = serde_json::Value::Null;
        let out =
            process_for_browserslist("stylex.css", css, dir.path(), "development", &none).unwrap();
        assert!(out.contains("-webkit-user-select"), "{out}");
        assert!(out.contains('\n'), "printed unminified: {out}");
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"browserslist":{"production":["chrome 120"],"development":["safari 12"]}}"#,
        )
        .unwrap();
        let prod =
            process_for_browserslist("stylex.css", css, dir.path(), "production", &none).unwrap();
        assert!(!prod.contains("-webkit-"), "{prod}");
        let dev =
            process_for_browserslist("stylex.css", css, dir.path(), "development", &none).unwrap();
        assert!(dev.contains("-webkit-user-select"), "{dev}");
        // lightningcssOptions: its own targets win over browserslist, and
        // `minify` prints compactly.
        let opts = serde_json::json!({ "targets": { "chrome": 120 << 16 }, "minify": true });
        let own =
            process_for_browserslist("stylex.css", css, dir.path(), "development", &opts).unwrap();
        assert_eq!(own, ".a{user-select:none}");
    }

    #[test]
    fn browser_targets_follow_vite_convert_targets() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let b = browser_targets(&s(&["chrome120", "safari16.4", "ios15", "node18"]))
            .browsers
            .unwrap();
        assert_eq!(b.chrome, Some(120 << 16));
        assert_eq!(b.safari, Some((16 << 16) | (4 << 8)));
        assert_eq!(b.ios_saf, Some(15 << 16));
        assert_eq!(b.firefox, None);
        // The lowest version per browser wins.
        let b = browser_targets(&s(&["chrome120", "chrome100"]))
            .browsers
            .unwrap();
        assert_eq!(b.chrome, Some(100 << 16));
        // No browser at all (esnext, empty) is Vite's baseline default.
        for list in [s(&["esnext"]), Vec::new()] {
            let b = browser_targets(&list).browsers.unwrap();
            assert_eq!(b.chrome, Some(111 << 16));
            assert_eq!(b.safari, Some((16 << 16) | (4 << 8)));
        }
    }

    #[test]
    fn es_year_targets_expand_like_vite_es_map() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let b = browser_targets(&s(&["es2022"])).browsers.unwrap();
        assert_eq!(b.chrome, Some(94 << 16));
        assert_eq!(b.edge, Some(94 << 16));
        assert_eq!(b.firefox, Some(93 << 16));
        assert_eq!(b.safari, Some((16 << 16) | (4 << 8)));
        assert_eq!(b.ios_saf, Some((16 << 16) | (4 << 8)));
        assert_eq!(b.opera, Some(80 << 16));
        assert_eq!(
            browser_targets(&s(&["es6"])).browsers.unwrap().chrome,
            Some(49 << 16)
        );
        // Mixed with browsers, the lowest version per browser still wins.
        let b = browser_targets(&s(&["chrome120", "es2020"]))
            .browsers
            .unwrap();
        assert_eq!(b.chrome, Some(80 << 16));
        // A year Vite has no browsers for is skipped, leaving the baseline.
        let b = browser_targets(&s(&["es2099"])).browsers.unwrap();
        assert_eq!(b.chrome, Some(111 << 16));
    }

    #[test]
    fn an_es_year_target_lowers_css_for_its_browsers() {
        let css = "@media (width <= 600px) { .a { color: red } }";
        // The baseline (safari 16.4, chrome 111) keeps range syntax ...
        let baseline = compile_css("/p.css", css, true).unwrap().css;
        assert!(baseline.contains("width<=600px"), "{baseline}");
        // ... `build.target: 'es2022'` means chrome 94, which lacks it.
        let es2022 = CssResolveConfig {
            targets: vec!["es2022".into()],
            minify: true,
            ..Default::default()
        };
        let lowered = compile_css_with("/p.css", css, &es2022.as_ref())
            .unwrap()
            .css;
        assert!(lowered.contains("max-width:600px"), "{lowered}");
    }

    #[test]
    fn autoprefixing_applies_for_targets() {
        let out = compile_css("/p.css", ".x { user-select: none; }", true).unwrap();
        assert!(
            out.css.contains("-webkit-user-select"),
            "autoprefixed: {}",
            out.css
        );
    }

    #[test]
    fn the_browser_matrix_keeps_modern_syntax_and_downlevels_the_rest() {
        // The target matrix is the compatibility contract of every stylesheet oj
        // emits. Asserted through behaviour: syntax the configured versions
        // support has to survive, and syntax they do not has to be lowered. A
        // matrix that decoded to version 0 would downlevel everything.
        let out = compile_css(
            "/p.css",
            ".a { width: clamp(1px, 2vw, 3px); color: rgb(0 0 0 / 50%); aspect-ratio: 1/2 }",
            true,
        )
        .unwrap()
        .css;
        assert!(out.contains("clamp("), "clamp is supported: {out}");
        assert!(out.contains("#00000080"), "modern color syntax: {out}");
        assert!(
            out.contains("aspect-ratio"),
            "aspect-ratio is supported: {out}"
        );
        assert!(!out.contains("max(1px"), "clamp must not be lowered: {out}");

        // Nesting is not supported by the oldest baseline target (safari 16.4),
        // so it is lowered; logical properties are (safari 15+), so they stay,
        // as Vite's default target keeps them.
        let nested = compile_css("/p.css", ".a { .b { color: red } }", true)
            .unwrap()
            .css;
        assert_eq!(nested, ".a .b{color:red}");
        let logical = compile_css("/p.css", ".a { inset-inline-start: 1px }", true)
            .unwrap()
            .css;
        assert_eq!(logical, ".a{inset-inline-start:1px}");
        // An older explicit target (Vite's legacy `modules` preset, safari14)
        // lowers them.
        let legacy = CssResolveConfig {
            targets: vec!["safari14".into()],
            minify: true,
            ..Default::default()
        };
        let lowered =
            compile_css_with("/p.css", ".a { inset-inline-start: 1px }", &legacy.as_ref())
                .unwrap()
                .css;
        assert!(
            lowered.contains("left:1px"),
            "logical props lowered for safari14: {lowered}"
        );
    }
}
