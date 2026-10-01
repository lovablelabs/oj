//! The lightningcss compile: parse with recovery, lower to targets, print,
//! then rebase urls, append the source map and build the module exports.

use std::sync::{Arc, RwLock};

use base64::Engine as _;
use lightningcss::css_modules;
use lightningcss::dependencies::DependencyOptions;
use lightningcss::error::{Error as CssError, ParserError};
use lightningcss::printer::PrinterOptions;
use lightningcss::stylesheet::{MinifyOptions, ParserOptions, StyleSheet};

use crate::modules::{is_css_module, module_exports, module_is_scoped, scoped_name_pattern};
use crate::rebase::{css_base_dir, dev_url_of, rewrite_dependencies};
use crate::resolve::CssResolve;
use crate::targets::browser_targets;

#[derive(Debug)]
pub struct CssOutput {
    pub css: String,
    /// A CSS module's class map, sorted by name; None for a plain stylesheet.
    pub exports: Option<Vec<(String, String)>>,
}

/// What a compile does beyond parse-and-print.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Mode {
    pub minify: bool,
    /// Rewrite relative urls to server-absolute ones (dev).
    pub rebase: bool,
    /// Append an inline source map (`css.devSourcemap`).
    pub source_map: bool,
}

pub fn compile_css(url: &str, source: &str, minify: bool) -> Result<CssOutput, String> {
    let mode = Mode {
        minify,
        ..Mode::default()
    };
    compile_css_depth(url, source, mode, &CssResolve::default(), 0)
}

/// The build compile: `build.cssMinify` and `build.cssTarget` from `resolve`.
pub fn compile_css_with(
    url: &str,
    source: &str,
    resolve: &CssResolve<'_>,
) -> Result<CssOutput, String> {
    let mode = Mode {
        minify: resolve.minify,
        ..Mode::default()
    };
    compile_css_depth(url, source, mode, resolve, 0)
}

pub fn compile_css_rebased(url: &str, source: &str, minify: bool) -> Result<CssOutput, String> {
    let mode = Mode {
        minify,
        rebase: true,
        source_map: false,
    };
    compile_css_depth(url, source, mode, &CssResolve::default(), 0)
}

/// `compile_css_rebased` plus an inline source map back to `url`.
pub fn compile_css_rebased_with_map(
    url: &str,
    source: &str,
    minify: bool,
) -> Result<CssOutput, String> {
    let mode = Mode {
        minify,
        rebase: true,
        source_map: true,
    };
    compile_css_depth(url, source, mode, &CssResolve::default(), 0)
}

/// The dev-server compile: urls rebased to server-absolute ones, aliased ones
/// to the url of the file they name, as Vite's url rewriter does.
pub fn compile_css_dev(
    url: &str,
    source: &str,
    source_map: bool,
    resolve: &CssResolve<'_>,
) -> Result<CssOutput, String> {
    let mode = Mode {
        minify: false,
        rebase: true,
        source_map,
    };
    compile_css_depth(url, source, mode, resolve, 0)
}

/// Parse with error recovery: like Vite's postcss pipeline, a legacy hack
/// (`*zoom: 1`, IE `filter: progid:...`) or a stray invalid rule is dropped
/// with a warning instead of failing the file.
pub(crate) fn parse_stylesheet<'i>(
    source: &'i str,
    filename: &str,
    css_modules: Option<css_modules::Config>,
) -> Result<StyleSheet<'i>, String> {
    let warnings = Arc::new(RwLock::new(Vec::new()));
    let options = ParserOptions {
        filename: filename.to_string(),
        css_modules,
        error_recovery: true,
        warnings: Some(Arc::clone(&warnings)),
        ..ParserOptions::default()
    };
    let sheet = StyleSheet::parse(source, options)
        .map_err(|err| format!("css parse error in {filename}: {err}"))?;
    report_warnings(filename, &warnings);
    Ok(sheet)
}

/// Recovered errors, printed like PostCSS plugin warnings.
fn report_warnings(name: &str, warnings: &RwLock<Vec<CssError<ParserError<'_>>>>) {
    const SHOWN: usize = 5;
    let Ok(list) = warnings.read() else {
        return;
    };
    for w in list.iter().take(SHOWN) {
        eprintln!("oj: css warning in {name}: {w}");
    }
    if list.len() > SHOWN {
        eprintln!("oj: css warning in {name}: {} more", list.len() - SHOWN);
    }
}

pub(crate) fn compile_css_depth(
    url: &str,
    source: &str,
    mode: Mode,
    resolve: &CssResolve<'_>,
    depth: u8,
) -> Result<CssOutput, String> {
    let is_module_file = is_css_module(url);
    let modules = (is_module_file && module_is_scoped(url, resolve)).then(|| css_modules::Config {
        pattern: scoped_name_pattern(resolve.modules),
        ..css_modules::Config::default()
    });
    let mut stylesheet = parse_stylesheet(source, url, modules)?;

    let targets = browser_targets(resolve.targets);
    stylesheet
        .minify(MinifyOptions {
            targets,
            ..MinifyOptions::default()
        })
        .map_err(|err| format!("css transform error in {url}: {err}"))?;

    let base = if mode.rebase { css_base_dir(url) } else { None };
    let mut sm = parcel_sourcemap::SourceMap::new("");
    if mode.source_map {
        let idx = sm.add_source(url);
        let _ = sm.set_source_content(idx as usize, source);
    }
    let result = stylesheet
        .to_css(PrinterOptions {
            minify: mode.minify,
            targets,
            analyze_dependencies: base.as_ref().map(|_| DependencyOptions::default()),
            source_map: mode.source_map.then_some(&mut sm),
            ..PrinterOptions::default()
        })
        .map_err(|err| format!("css print error in {url}: {err}"))?;

    let mut css = match (base, result.dependencies) {
        (Some(base), Some(deps)) => {
            rewrite_dependencies(result.code, deps, |spec| dev_url_of(&spec, &base, resolve))
        }
        _ => result.code,
    };
    if mode.source_map {
        // `sourceRoot: "/"` makes devtools resolve `src/app.css` to the served url.
        let json = sm
            .to_json(Some("/"))
            .map_err(|err| format!("css sourcemap error in {url}: {err}"))?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(json);
        css.push_str(&format!(
            "\n/*# sourceMappingURL=data:application/json;base64,{encoded} */\n"
        ));
    }

    let exports = match result.exports {
        Some(map) => Some(module_exports(map, url, resolve, depth)),
        // A module compiled in global mode still exports an (empty) map.
        None if is_module_file => Some(Vec::new()),
        None => None,
    };
    Ok(CssOutput { css, exports })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;
    use std::path::Path;

    #[test]
    fn plain_css_passes_through_and_minifies() {
        let out = compile_css("/styles.css", "body {\n  color: red;\n}\n", true).unwrap();
        assert_eq!(out.css, "body{color:red}");
        assert!(out.exports.is_none());
    }

    #[test]
    fn css_target_and_minify_settings_drive_the_build_compile() {
        let src = ".a {\n  .b { color: red }\n}\n";
        let modern = CssResolveConfig {
            targets: vec!["chrome120".into(), "safari17.2".into(), "firefox117".into()],
            minify: true,
            ..Default::default()
        };
        let out = compile_css_with("/a.css", src, &modern.as_ref())
            .unwrap()
            .css;
        assert!(
            out.contains(".a{") && out.contains(".b{"),
            "nesting kept for targets that support it: {out}"
        );
        assert!(!out.contains(".a .b"), "{out}");

        let baseline = CssResolveConfig {
            minify: true,
            ..Default::default()
        };
        let out = compile_css_with("/a.css", src, &baseline.as_ref())
            .unwrap()
            .css;
        assert_eq!(
            out, ".a .b{color:red}",
            "baseline (safari16.4) lowers nesting"
        );

        let unminified = CssResolveConfig {
            minify: false,
            ..Default::default()
        };
        let out = compile_css_with("/a.css", src, &unminified.as_ref())
            .unwrap()
            .css;
        assert!(
            out.contains('\n') && out.contains("color: red"),
            "cssMinify false keeps whitespace: {out}"
        );
    }

    #[test]
    fn rebases_relative_url_and_import_to_server_root() {
        // A stylesheet served at /src/app.css: its relative @import and url()
        // must become server-absolute so an injected <style> resolves them
        // against the server root, not the page URL.
        let src = "@import \"./base.css\";\n.a { background: url(./img/bg.png); }";
        let out = compile_css_rebased("/src/app.css", src, true).unwrap();
        assert!(
            out.css.contains("/src/base.css"),
            "import rebased: {}",
            out.css
        );
        assert!(
            out.css.contains("/src/img/bg.png"),
            "url rebased: {}",
            out.css
        );
        assert!(
            !out.css.contains("./"),
            "no relative refs remain: {}",
            out.css
        );
    }

    #[test]
    fn rebase_resolves_parent_segments_and_skips_external_urls() {
        let src = ".a { background: url(../assets/x.png); }\n\
                   .b { background: url(https://cdn.test/y.png); }\n\
                   .c { background: url(data:image/png;base64,AAAA); }";
        let out = compile_css_rebased("/src/ui/card.css", src, true).unwrap();
        assert!(
            out.css.contains("/src/assets/x.png"),
            "parent rebased: {}",
            out.css
        );
        assert!(
            out.css.contains("https://cdn.test/y.png"),
            "external kept: {}",
            out.css
        );
        assert!(
            out.css.contains("data:image/png;base64,AAAA"),
            "data URI kept: {}",
            out.css
        );
    }

    #[test]
    fn plain_compile_does_not_rebase_urls() {
        // The build/SSR path must be untouched: url() stays relative.
        let src = ".a { background: url(./bg.png); }";
        let out = compile_css("/src/app.css", src, true).unwrap();
        assert!(
            out.css.contains("./bg.png"),
            "non-rebased keeps the relative url: {}",
            out.css
        );
    }

    #[test]
    fn dev_sourcemap_is_appended_inline_and_names_the_source() {
        let out = compile_css_rebased_with_map(
            "/src/app.css",
            ".a {\n  color: red;\n}\n.b { color: blue; }\n",
            false,
        )
        .unwrap();
        let marker = "/*# sourceMappingURL=data:application/json;base64,";
        let at = out.css.find(marker).expect("sourceMappingURL comment");
        let b64_json = out.css[at + marker.len()..]
            .trim_end()
            .trim_end_matches("*/")
            .trim();
        // Decode the base64 back and check the map shape.
        let decoded = {
            let t = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut bits = 0u32;
            let mut n = 0;
            let mut bytes = Vec::new();
            for c in b64_json.bytes().filter(|c| *c != b'=') {
                let v = t.iter().position(|x| *x == c).unwrap() as u32;
                bits = bits << 6 | v;
                n += 6;
                if n >= 8 {
                    n -= 8;
                    bytes.push((bits >> n) as u8);
                    bits &= (1 << n) - 1;
                }
            }
            String::from_utf8(bytes).unwrap()
        };
        assert!(
            decoded.contains("\"sources\":[\"src/app.css\"]")
                && decoded.contains("\"sourceRoot\":\"/\""),
            "{decoded}"
        );
        assert!(
            decoded.contains("sourcesContent"),
            "source embedded: {decoded}"
        );
        assert!(
            decoded.contains("\"mappings\":\"") && !decoded.contains("\"mappings\":\"\""),
            "non-empty mappings: {decoded}"
        );
        // Without the flag nothing is appended.
        assert!(
            !compile_css_rebased("/src/app.css", ".a { color: red; }", false)
                .unwrap()
                .css
                .contains("sourceMappingURL")
        );
    }

    #[test]
    fn dev_compile_rewrites_aliased_urls_and_keeps_root_absolute_ones() {
        let alias = vec![("@".to_string(), "./src".to_string())];
        let r = CssResolve {
            root: Some(Path::new("/proj")),
            public_dir: None,
            alias: &alias,
            ..CssResolve::default()
        };
        let src = ".a { background: url(@/img/bg.png?v=1); }\n\
                   .b { background: url(/src/x.png); }\n\
                   .c { background: url(./y.png); }\n\
                   .d { background: url(/logo.svg#id); }";
        let out = compile_css_dev("/src/ui/card.css", src, false, &r)
            .unwrap()
            .css;
        assert!(
            out.contains("url(\"/src/img/bg.png?v=1\")")
                || out.contains("url(/src/img/bg.png?v=1)"),
            "aliased url -> served url of the file: {out}"
        );
        assert!(out.contains("/src/x.png"), "root-absolute kept: {out}");
        assert!(
            !out.contains("/src/@/"),
            "alias must not be treated as a relative segment: {out}"
        );
        assert!(
            out.contains("/src/ui/y.png"),
            "relative still rebased: {out}"
        );
        assert!(out.contains("/logo.svg#id"), "public url kept: {out}");
        // An alias to a file outside the root is served through /@fs.
        let outside = vec![("~ui".to_string(), "/elsewhere/ui".to_string())];
        let r2 = CssResolve {
            root: Some(Path::new("/proj")),
            public_dir: None,
            alias: &outside,
            ..CssResolve::default()
        };
        let out = compile_css_dev(
            "/src/a.css",
            ".a { background: url(~ui/i.png) }",
            false,
            &r2,
        )
        .unwrap()
        .css;
        assert!(out.contains("/@fs/elsewhere/ui/i.png"), "{out}");
    }

    #[test]
    fn parse_errors_are_recovered_not_panicked() {
        // Vite's postcss pipeline never fails a stylesheet over garbage it
        // cannot parse; the invalid rule is dropped and nothing else is lost.
        let out = compile_css("/x.css", "!!not-css!! {}\n.ok { color: red }", true).unwrap();
        assert_eq!(out.css, ".ok{color:red}");
    }

    #[test]
    fn legacy_hacks_do_not_fail_the_stylesheet() {
        // Bootstrap-3 era vendor CSS: `*zoom`, `_height` and IE `filter:
        // progid:` are declaration-level hacks postcss keeps and Vite serves.
        // lightningcss keeps `_height` and `progid:` verbatim but cannot parse
        // the star hack; with error recovery it drops just that declaration
        // and the rest of the rule (and file) survives.
        let src = ".clearfix { *zoom: 1; _height: 1px; color: red; }\n\
                   .g { filter: progid:DXImageTransform.Microsoft.gradient(startColorstr='#fff', endColorstr='#000'); background: blue; }\n\
                   .b { color: green; }";
        let out = compile_css("/vendor.css", src, true).unwrap();
        assert!(
            out.css.contains(".clearfix{_height:1px;color:red}"),
            "{}",
            out.css
        );
        assert!(
            out.css.contains("progid:DXImageTransform") && out.css.contains("background:#00f"),
            "{}",
            out.css
        );
        assert!(out.css.contains(".b{color:green}"), "{}", out.css);
        // The rebased (dev) path parses the same way.
        assert!(compile_css_rebased("/src/vendor.css", src, false).is_ok());
    }
}
