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

/// esbuild-style targets as lightningcss browsers, like Vite's
/// `convertTargets`: the lowest version per browser wins, non-browser names
/// (`es2020`, `node18`) are skipped, and no browser at all means the baseline.
pub fn browser_targets(list: &[String]) -> Targets {
    let browsers = parse_browsers(list.iter().map(String::as_str)).or_else(|| {
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
        let b = browser_targets(&s(&[
            "chrome120",
            "safari16.4",
            "es2020",
            "ios15",
            "node18",
        ]))
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
