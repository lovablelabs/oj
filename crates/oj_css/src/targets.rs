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
