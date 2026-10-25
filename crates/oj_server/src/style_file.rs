//! One stylesheet file compiled as a module, the way Vite's compileCSS runs
//! it: the preprocessor for its language first (Sass, Less, Stylus, under
//! `css.preprocessorOptions`), then CSS-module scoping for a `.module.<lang>`
//! file. Dev SSR, the Start dev server and the Start bundles all compile style
//! files here, so they agree on the stylesheet and on its class names.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use regex::Regex;

use crate::css_engine::CssEngine;
use crate::plugins::PluginHost;

/// One Vite environment's plugins and the id they see a stylesheet under,
/// for the transform slots around Vite's core CSS plugins: "pre" on the
/// source, "normal" on the CSS vite:css makes of it, "post" on the module
/// vite:css-post makes of that.
#[derive(Clone, Copy)]
pub struct StyleSlots<'a> {
    pub host: &'a PluginHost,
    pub id: &'a str,
}

/// A stylesheet's source: what a plugin `load` returns for its id (Vite runs
/// load before the fs read), else the file.
pub async fn read_style_source(
    slots: Option<StyleSlots<'_>>,
    path: &Path,
) -> Result<String, String> {
    if let Some(StyleSlots { host, id }) = slots {
        if host.hook_wants_load(id) {
            if let Some(code) = host
                .load(id)
                .await
                .map_err(|e| format!("plugin load error for {id}:\n{e}"))?
            {
                return Ok(code);
            }
        }
    }
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// One slot (`"pre"`, `"normal"` or `"post"`) of a stylesheet's plugin
/// transforms; a throwing plugin fails it, as in Vite.
pub async fn style_slot(
    slots: Option<StyleSlots<'_>>,
    code: String,
    phase: &str,
) -> Result<String, String> {
    let Some(StyleSlots { host, id }) = slots else {
        return Ok(code);
    };
    if !host.hook_wants_transform(id, &code) {
        return Ok(code);
    }
    host.transform_phase(&code, id, phase)
        .await
        .map(|t| t.code)
        .map_err(|e| format!("plugin transform error for {id}:\n{e}"))
}

/// A stylesheet as a module: its JS (a CSS module's class map, else an empty
/// module) and the compiled sheet, urls left as written.
pub struct StyleModule {
    pub code: String,
    pub css: String,
}

/// What a compile reads from the app.
pub struct StyleInputs<'a> {
    pub root: &'a Path,
    pub css: Option<&'a oj_config::CssConfig>,
    pub resolve: oj_css::CssResolve<'a>,
}

/// Less and Stylus run on the preprocess engine; the caller passes it for
/// such a file.
pub fn needs_preprocess_engine(path: &Path) -> bool {
    let url = path.to_string_lossy();
    crate::sidecar::preprocess_lang(&url).is_some()
}

pub async fn compile_style_file(
    inputs: &StyleInputs<'_>,
    path: &Path,
    source: String,
    preprocess: Option<&CssEngine>,
) -> Result<StyleModule, String> {
    let mut sss_source = None;
    let url = crate::rewrite::url_of(inputs.root, path);
    let config = inputs.css.map(|c| oj_config::OjConfig {
        css: Some(c.clone()),
        ..Default::default()
    });
    let css = if oj_css::is_sass(&url) {
        let lang = if url.ends_with(".sass") {
            "sass"
        } else {
            "scss"
        };
        let data = config
            .as_ref()
            .and_then(|c| oj_config::css_additional_data(c, lang));
        let load_paths: Vec<PathBuf> = config
            .as_ref()
            .map(|c| oj_config::css_load_paths(c, lang))
            .unwrap_or_default()
            .into_iter()
            .map(|p| inputs.root.join(p))
            .collect();
        oj_css::compile_sass_collecting(
            &source,
            &oj_css::SassOptions {
                load_dir: path.parent(),
                additional_data: data.as_deref(),
                load_paths: &load_paths,
                resolve: inputs.resolve,
            },
            &mut Vec::new(),
        )?
    } else if needs_preprocess_engine(path) {
        if crate::sidecar::is_sugarss(&url) {
            sss_source = Some(source.clone());
        }
        let engine = preprocess.ok_or_else(|| format!("{url}: no css preprocessor"))?;
        let lang = crate::sidecar::preprocess_lang(&url).unwrap_or("stylus");
        let (data, opts) = match &config {
            Some(c) => (
                oj_config::css_additional_data(c, lang),
                oj_config::css_preprocessor_json(c, lang),
            ),
            None => (None, serde_json::Value::Null),
        };
        let opts = match lang {
            "sss" => crate::sidecar::sss_engine_options(&inputs.resolve),
            _ => opts,
        };
        let with_data = match data {
            Some(d) if !d.is_empty() => format!("{d}\n{source}"),
            _ => source,
        };
        engine
            .compile_with(&with_data, &url, opts)
            .await
            .map_err(|e| format!("css preprocess error for {url}: {e}"))?
    } else {
        source
    };
    let resolve = oj_css::CssResolve {
        minify: false,
        ..inputs.resolve
    };
    // PostCSS parses a SugarSS sheet itself: its module names hash that text.
    let output = match &sss_source {
        Some(raw) => oj_css::compile_css_with_scope_source(&url, &css, raw, &resolve)?,
        None => oj_css::compile_css_with(&url, &css, &resolve)?,
    };
    let code = match output.exports {
        Some(exports) => oj_css::css_modules_esm(&exports),
        None => "export default {};".to_string(),
    };
    Ok(StyleModule {
        code,
        css: output.css,
    })
}

/// The sheets the app's Tailwind compiles (by its markers), in the Start dev
/// server and the Start bundles alike (needsCssCompile there).
pub fn needs_css_compile(src: &str) -> bool {
    src.contains("tailwindcss")
        || src.contains("@tailwind")
        || src.contains("@plugin")
        || src.contains("@apply")
}

/// The prefix Start's dev urls take under Vite's `base` (`/app/` gives
/// `/app`), as the dev server resolves it (`dev_base`): empty at the
/// root and for a relative base, the path of a full-url one.
pub fn start_dev_url_base(base: &str) -> String {
    crate::middleware::dev_base(Some(base))
        .map(|b| b.trim_end_matches('/').to_string())
        .unwrap_or_default()
}

/// Where Start's unbundled dev serves a file (the Start bundles' fsBase),
/// under `url_base` (see [`start_dev_url_base`]).
pub fn start_dev_file_url(url_base: &str, file: &Path) -> String {
    format!("{url_base}/@oj-start/fs{}", file.display())
}

/// Vite's `?inline` stylesheet under Start dev: the module whose default
/// export is the compiled sheet (css.ts: preprocessor, CSS-module scoping,
/// then the app's postcss plugins, here Tailwind), its relative urls pointed
/// at the files' dev URLs. SSR and the dev client bundle both build it here,
/// so a rendered `<style>` hydrates as is. `tailwind` is asked for only when
/// the sheet needs it. `slots`: the environment's "pre" and "normal" plugin
/// transforms, around that compile (the url rewrite is vite:css's too).
#[allow(clippy::too_many_arguments)]
pub async fn start_dev_inline_style(
    inputs: &StyleInputs<'_>,
    path: &Path,
    source: String,
    preprocess: Option<&CssEngine>,
    tailwind: impl AsyncFnOnce() -> Result<Arc<CssEngine>, String>,
    url_base: &str,
    slots: Option<StyleSlots<'_>>,
) -> Result<String, String> {
    let source = style_slot(slots, source, "pre").await?;
    let mut css = compile_style_file(inputs, path, source, preprocess)
        .await?
        .css;
    if needs_css_compile(&css) {
        css = tailwind()
            .await?
            .compile_path(&css, path, serde_json::Value::Null, true)
            .await?;
    }
    let dir = path.parent().unwrap_or(Path::new("/"));
    let css = rewrite_relative_urls(&css, dir, |file| start_dev_file_url(url_base, file));
    let css = style_slot(slots, css, "normal").await?;
    Ok(format!(
        "export default {};",
        serde_json::Value::String(css)
    ))
}

/// Relative `url()`s naming an existing file, rewritten to `url_for` it; the
/// Start bundles' rewriteCss rule (data:, remote, fragment and root-absolute
/// urls stay; a missing file stays as written).
pub fn rewrite_relative_urls(css: &str, dir: &Path, url_for: impl Fn(&Path) -> String) -> String {
    static URL_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"url\(\s*(['"]?)([^'")]+)['"]?\s*\)"#).unwrap());
    static KEEP_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(data:|https?:|//|#|/)").unwrap());
    URL_RE
        .replace_all(css, |caps: &regex::Captures<'_>| {
            let target = caps[2].trim();
            if KEEP_RE.is_match(target) {
                return caps[0].to_string();
            }
            let clean = target.split(['?', '#']).next().unwrap_or(target);
            let file = oj_css::normalize(&dir.join(clean));
            if !file.is_file() {
                return caps[0].to_string();
            }
            let suffix = &target[clean.len()..];
            format!(
                "url({})",
                serde_json::Value::String(format!("{}{suffix}", url_for(&file)))
            )
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_css_compile_detects_tailwind_markers() {
        assert!(needs_css_compile("@import \"tailwindcss\";"));
        assert!(needs_css_compile("@tailwind base;"));
        assert!(needs_css_compile("@plugin \"@tailwindcss/typography\";"));
        assert!(needs_css_compile(".btn { @apply px-2; }"));
        assert!(!needs_css_compile(".a { color: red }"));
        assert!(!needs_css_compile("@font-face { src: url(x.woff2) }"));
    }

    #[test]
    fn relative_urls_point_at_existing_files_only() {
        let dir = std::env::temp_dir().join(format!("oj-inline-urls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.png"), b"x").unwrap();
        let out = rewrite_relative_urls(
            ".a{background:url(./a.png?v=1)} .b{background:url('missing.png')} .c{background:url(data:x)} .d{background:url(/abs.png)}",
            &dir,
            |f| format!("/fs{}", f.display()),
        );
        let a = format!("url(\"/fs{}?v=1\")", dir.join("a.png").display());
        assert!(out.contains(&a), "{out}");
        assert!(out.contains("url('missing.png')"));
        assert!(out.contains("url(data:x)"));
        assert!(out.contains("url(/abs.png)"));
    }
}
