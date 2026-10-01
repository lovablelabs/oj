//! Sass through grass, with the resolution Vite's sass importer adds: dotted
//! basenames, npm package entries, aliases, root-absolute paths and `~pkg`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::path::{node_modules_load_paths, read_package_json, with_ext};
use crate::resolve::{CssResolve, CssResolveConfig};

pub fn is_sass(url: &str) -> bool {
    let f = url.split('?').next().unwrap_or(url);
    f.ends_with(".scss") || f.ends_with(".sass")
}

/// How a Sass stylesheet is compiled: the importer's directory, configured
/// `loadPaths`, the `node_modules` above it, `additionalData`, and
/// alias / root-absolute resolution.
#[derive(Debug, Default, Clone, Copy)]
pub struct SassOptions<'a> {
    pub load_dir: Option<&'a Path>,
    pub additional_data: Option<&'a str>,
    pub load_paths: &'a [PathBuf],
    pub resolve: CssResolve<'a>,
}

pub fn compile_sass(source: &str, load_dir: Option<&Path>) -> Result<String, String> {
    compile_sass_with(source, load_dir, None)
}

pub fn compile_sass_with(
    source: &str,
    load_dir: Option<&Path>,
    additional_data: Option<&str>,
) -> Result<String, String> {
    compile_sass_opts(
        source,
        &SassOptions {
            load_dir,
            additional_data,
            ..SassOptions::default()
        },
    )
}

pub fn compile_sass_opts(source: &str, opts: &SassOptions<'_>) -> Result<String, String> {
    compile_sass_collecting(source, opts, &mut Vec::new())
}

/// `compile_sass_opts`, appending every stylesheet loaded to `deps` (sass's
/// `includedFiles`, Vite's HMR dependencies for the sheet).
pub fn compile_sass_collecting(
    source: &str,
    opts: &SassOptions<'_>,
    deps: &mut Vec<PathBuf>,
) -> Result<String, String> {
    let fs = SassFs {
        resolve: opts.resolve.into(),
        seen: RefCell::default(),
        probes: RefCell::default(),
    };
    let mut options = grass::Options::default().fs(&fs);
    let node_modules = opts
        .load_dir
        .map(node_modules_load_paths)
        .unwrap_or_default();
    for dir in opts
        .load_dir
        .iter()
        .copied()
        .chain(opts.load_paths.iter().map(PathBuf::as_path))
    {
        options = options.load_path(dir);
    }
    for nm in &node_modules {
        options = options.load_path(nm);
    }
    let prepared = prepare_sass_imports(source, &opts.resolve);
    let source = match opts.additional_data {
        Some(data) if !data.is_empty() => format!("{data}\n{prepared}"),
        _ => prepared,
    };
    let out = grass::from_string(source, &options).map_err(|e| format!("sass error: {e}"));
    deps.append(&mut fs.seen.borrow_mut());
    out
}

/// Rewrite `@use` / `@forward` / `@import` lines into specifiers grass can
/// resolve: `.scss` / `.sass` dropped (grass mis-probes dotted basenames that
/// keep it), aliases and root-absolute paths made absolute, `~` stripped.
/// `url(...)` lines and `.css` imports are left alone.
pub(crate) fn prepare_sass_imports(source: &str, resolve: &CssResolve<'_>) -> String {
    let has_resolve = resolve.root.is_some() || !resolve.alias.is_empty();
    let mut out = String::with_capacity(source.len());
    for line in source.split_inclusive('\n') {
        let t = line.trim_start();
        let is_import = ["@use", "@forward", "@import"]
            .iter()
            .any(|k| t.starts_with(k));
        if !is_import || line.contains("url(") {
            out.push_str(line);
            continue;
        }
        let stripped = line
            .replace(".scss\"", "\"")
            .replace(".scss'", "'")
            .replace(".sass\"", "\"")
            .replace(".sass'", "'");
        // Aliases first, so a configured `~` alias wins over the strip.
        let aliased = if has_resolve {
            rewrite_quoted(&stripped, |spec| rewrite_sass_spec(spec, resolve))
        } else {
            stripped
        };
        out.push_str(&aliased.replace("\"~", "\"").replace("'~", "'"));
    }
    out
}

/// Every quoted string of `line` passed through `rewrite` (None keeps it).
fn rewrite_quoted(line: &str, rewrite: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find(['"', '\'']) {
        let quote = rest.as_bytes()[open] as char;
        let Some(close) = rest[open + 1..].find(quote) else {
            break;
        };
        let inner = &rest[open + 1..open + 1 + close];
        out.push_str(&rest[..=open]);
        out.push_str(rewrite(inner).as_deref().unwrap_or(inner));
        out.push(quote);
        rest = &rest[open + close + 2..];
    }
    out.push_str(rest);
    out
}

/// An aliased or root-absolute specifier as an absolute path. Root-absolute
/// only when a stylesheet exists under the root (it may be a real absolute
/// path otherwise).
fn rewrite_sass_spec(spec: &str, resolve: &CssResolve<'_>) -> Option<String> {
    let path = match resolve.alias_path(spec) {
        Some(p) => p,
        None => resolve.root_path(spec).filter(|p| sass_file_exists(p))?,
    };
    Some(path.to_string_lossy().into_owned())
}

/// Whether `base` names a stylesheet the way dart-sass probes: the file, an
/// added extension, a `_partial`, or a directory index.
fn sass_file_exists(base: &Path) -> bool {
    if base.is_file() {
        return true;
    }
    let Some(name) = base.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let parent = base.parent().unwrap_or(Path::new("."));
    ["scss", "sass", "css"]
        .iter()
        .any(|ext| with_ext(base, ext).is_file() || parent.join(format!("_{name}.{ext}")).is_file())
        || INDEX_FILES.iter().any(|(i, _)| base.join(i).is_file())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Syntax {
    Scss,
    Sass,
}

impl Syntax {
    fn ext(self) -> &'static str {
        match self {
            Syntax::Scss => "scss",
            Syntax::Sass => "sass",
        }
    }

    /// Entry extensions a probe of this syntax accepts (grass parses by the
    /// probed extension, so `.sass` never reads scss and vice versa).
    fn entry_exts(self) -> &'static [&'static str] {
        match self {
            Syntax::Scss => &["scss", "css"],
            Syntax::Sass => &["sass"],
        }
    }
}

const INDEX_FILES: [(&str, Syntax); 4] = [
    ("index.scss", Syntax::Scss),
    ("_index.scss", Syntax::Scss),
    ("index.sass", Syntax::Sass),
    ("_index.sass", Syntax::Sass),
];

/// The syntax of a grass directory-index probe `.../X/index.scss`.
fn index_probe(p: &Path) -> Option<Syntax> {
    let name = p.file_name()?.to_str()?;
    INDEX_FILES
        .iter()
        .find(|(i, _)| *i == name)
        .map(|&(_, s)| s)
}

/// A grass `Fs` over the real one that fixes up resolution and records every
/// file read.
///
/// grass 0.13 reads a trailing `.segment` as an extension, so `@use
/// "vars.module"` never finds `vars.module.scss`. It does probe the basename as
/// a directory, so a dotted stylesheet is presented as a phantom directory
/// whose index is that file, which matches dart-sass. Directory probes into npm
/// packages resolve to the package.json `sass` / `style` / `main` entry.
#[derive(Debug)]
struct SassFs {
    /// Applied to the import lines of every file read, so nested imports
    /// resolve like the entry's.
    resolve: CssResolveConfig,
    seen: RefCell<Vec<PathBuf>>,
    /// `(is_dir, is_file)` answers: grass re-probes the same candidates from
    /// every partial, and files do not change within a compile.
    probes: RefCell<HashMap<(bool, PathBuf), bool>>,
}

impl SassFs {
    fn probe(&self, dir: bool, p: &Path, answer: impl FnOnce() -> bool) -> bool {
        let key = (dir, p.to_path_buf());
        if let Some(&hit) = self.probes.borrow().get(&key) {
            return hit;
        }
        let v = answer();
        self.probes.borrow_mut().insert(key, v);
        v
    }
}

impl grass::Fs for SassFs {
    fn is_dir(&self, p: &Path) -> bool {
        self.probe(true, p, || p.is_dir() || dotted_stylesheet(p).is_some())
    }

    fn is_file(&self, p: &Path) -> bool {
        self.probe(false, p, || p.is_file() || probe_target(p).is_some())
    }

    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        let real = if p.is_file() {
            p.to_path_buf()
        } else {
            probe_target(p).unwrap_or_else(|| p.to_path_buf())
        };
        let bytes = std::fs::read(&real)?;
        self.seen.borrow_mut().push(real);
        match String::from_utf8(bytes) {
            Ok(text) => Ok(prepare_sass_imports(&text, &self.resolve.as_ref()).into_bytes()),
            Err(e) => Ok(e.into_bytes()),
        }
    }
}

/// The real file behind a probe that is not itself a file.
fn probe_target(p: &Path) -> Option<PathBuf> {
    index_target(p).or_else(|| package_entry(p))
}

/// `.../X/index.scss` -> `.../X.scss` when `X` is a dotted stylesheet,
/// keeping the probed syntax.
fn index_target(p: &Path) -> Option<PathBuf> {
    let ext = index_probe(p)?.ext();
    let parent = p.parent()?;
    let direct = with_ext(parent, ext);
    if direct.is_file() {
        return Some(direct);
    }
    let c = with_ext(&collapse_phantom_dirs(parent)?, ext);
    c.is_file().then_some(c)
}

/// `.../pkg/index.scss` -> the package's `sass`, `style` or `main` entry, as
/// Vite's sass importer resolves a directory import.
fn package_entry(probe: &Path) -> Option<PathBuf> {
    let syntax = index_probe(probe)?;
    let dir = probe.parent()?;
    let pkg = read_package_json(dir)?;
    let exts = syntax.entry_exts();
    let accepts = |p: &Path| {
        p.is_file()
            && p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| exts.contains(&e))
    };
    ["sass", "style", "main"]
        .iter()
        .filter_map(|field| pkg.get(field)?.as_str())
        .find_map(|rel| {
            let p = dir.join(rel);
            if accepts(&p) {
                return Some(p);
            }
            exts.iter()
                .map(|ext| with_ext(&p, ext))
                .find(|c| c.is_file())
        })
}

/// `p.scss` / `p.sass` when it is a real file, also behind phantom dirs.
fn dotted_stylesheet(p: &Path) -> Option<PathBuf> {
    let collapsed = collapse_phantom_dirs(p);
    std::iter::once(p)
        .chain(collapsed.as_deref())
        .flat_map(|base| ["scss", "sass"].map(|ext| with_ext(base, ext)))
        .find(|c| c.is_file())
}

/// A relative import made from inside a dotted stylesheet carries its phantom
/// directory (`.../x.module/sibling`). Drop every intermediate segment that is
/// really a `seg.scss` / `seg.sass` file. Some only when something changed.
fn collapse_phantom_dirs(p: &Path) -> Option<PathBuf> {
    let comps: Vec<_> = p.components().collect();
    let mut out = PathBuf::new();
    let mut changed = false;
    for (i, comp) in comps.iter().enumerate() {
        let candidate = out.join(comp);
        let is_last = i + 1 == comps.len();
        // Only a dotted name can be a phantom dir (grass finds `seg.scss` itself).
        let dotted = matches!(comp, Component::Normal(n) if n.as_encoded_bytes().contains(&b'.'));
        if !is_last
            && dotted
            && (with_ext(&candidate, "scss").is_file() || with_ext(&candidate, "sass").is_file())
        {
            changed = true;
            continue;
        }
        out = candidate;
    }
    changed.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    fn strip_sass_import_ext(source: &str) -> String {
        prepare_sass_imports(source, &CssResolve::default())
    }

    #[test]
    fn is_sass_strips_query_and_checks_extension() {
        assert!(is_sass("/src/theme.scss"));
        assert!(is_sass("vars.sass"));
        assert!(is_sass("/a/theme.scss?inline"));
        assert!(!is_sass("/a/theme.css"));
        assert!(!is_sass("/a/scss.ts"));
    }

    #[test]
    fn sass_nesting_and_variables_compile() {
        let scss = "$pad: 1rem;\n.card { padding: $pad; .title { font-weight: bold; } }";
        let css = compile_sass(scss, None).unwrap();
        assert!(css.contains("padding: 1rem"), "variable resolved: {css}");
        assert!(css.contains(".card .title"), "nesting flattened: {css}");
    }

    #[test]
    fn sass_then_lightningcss_pipeline() {
        let css = compile_sass(".a { .b { color: red } }", None).unwrap();
        let out = compile_css("/x.scss", &css, true).unwrap();
        assert!(out.css.contains(".a .b{color:red}"), "{}", out.css);
    }

    #[test]
    fn additional_data_is_prepended_before_compiling() {
        // css.preprocessorOptions.scss.additionalData: a global variable the
        // stylesheet never declares must resolve because it is injected first.
        let scss = ".btn { color: $brand; }";
        let css = compile_sass_with(scss, None, Some("$brand: #f00;")).unwrap();
        assert!(css.contains("color: #f00"), "injected var resolved: {css}");
        // Without the injection the same source fails (undefined variable).
        assert!(
            compile_sass(scss, None).is_err(),
            "undeclared variable must fail without additionalData",
        );
        // Empty / absent additionalData leaves compilation unchanged.
        assert!(compile_sass_with(".a { color: red; }", None, Some("")).is_ok());
    }

    // grass 0.13 mis-reads a dotted basename (`variables.module`) as an
    // extension and never finds `variables.module.scss`; the DottedFs shim
    // makes the CSS-modules `.module.scss` convention resolve like dart-sass.
    #[test]
    fn sass_resolves_dotted_module_import() {
        let base = std::env::temp_dir().join(format!("oj-css-dotmod-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("comp")).unwrap();
        std::fs::create_dir_all(base.join("css")).unwrap();
        std::fs::write(base.join("css/variables.module.scss"), "$c: #f00;").unwrap();
        // a plain (non-dotted) sibling must keep working too
        std::fs::write(base.join("css/plain.scss"), "$p: 2px;").unwrap();
        // Both the bare and the explicit-extension forms must resolve, matching
        // dart-sass / Vite (verified against dart-sass 1.51: both give the same
        // output). Excalidraw uses both across its stylesheets.
        for spec in ["../css/variables.module", "../css/variables.module.scss"] {
            let src = format!(
                "@use \"{spec}\" as v;\n@use \"../css/plain\" as p;\n.x {{ color: v.$c; margin: p.$p; }}"
            );
            let css = compile_sass(&src, Some(&base.join("comp")))
                .unwrap_or_else(|e| panic!("`{spec}` should resolve: {e}"));
            assert!(
                css.contains("color: red") || css.contains("#f00"),
                "{spec}: {css}"
            );
            assert!(css.contains("margin: 2px"), "{spec}: {css}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    // A dotted `.module.scss` that `@use`s another dotted module which itself
    // `@use`s a relative dotted sibling: the nested relative import must resolve
    // against the imported file's real directory, not the phantom `x.module/`
    // directory grass sees it through. (The CSS-modules pattern in the wild.)
    #[test]
    fn sass_resolves_nested_relative_dotted_import() {
        let base = std::env::temp_dir().join(format!("oj-css-nested-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("comp")).unwrap();
        std::fs::create_dir_all(base.join("shared")).unwrap();
        std::fs::write(base.join("shared/colors.module.scss"), "$c: #0f0;").unwrap();
        // base.module.scss imports a relative dotted sibling (colors.module).
        std::fs::write(
            base.join("shared/base.module.scss"),
            "@use \"colors.module\" as c;\n.base { color: c.$c; }",
        )
        .unwrap();
        // The entry (in comp/) imports base via a dir-relative path.
        let src = "@use \"../shared/base.module.scss\";\n.x { display: block; }";
        let css = compile_sass(src, Some(&base.join("comp")))
            .unwrap_or_else(|e| panic!("nested dotted @use should resolve: {e}"));
        assert!(
            css.contains("color: #0f0") || css.contains("color: green"),
            "{css}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sass_resolves_node_modules_packages_like_vite() {
        // `@use "pkg"` -> package.json `sass` (then `style`) entry; `@use
        // "pkg/path"` -> a file inside the package; `~pkg` is accepted; the
        // node_modules directory is found above the importing file.
        let base = std::env::temp_dir().join(format!("oj-css-nm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let pkg = base.join("node_modules/@acme/tokens");
        std::fs::create_dir_all(pkg.join("src")).unwrap();
        std::fs::create_dir_all(base.join("src/deep")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"@acme/tokens","main":"index.js","sass":"src/index.scss"}"#,
        )
        .unwrap();
        std::fs::write(pkg.join("src/index.scss"), "$brand: #123456;\n").unwrap();
        std::fs::write(
            pkg.join("src/_mixins.scss"),
            "@mixin pad { padding: 4px; }\n",
        )
        .unwrap();
        let styled = base.join("node_modules/plain-css");
        std::fs::create_dir_all(&styled).unwrap();
        std::fs::write(
            styled.join("package.json"),
            r#"{"name":"plain-css","style":"dist/x.css"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(styled.join("dist")).unwrap();
        std::fs::write(styled.join("dist/x.css"), ".plain { color: green; }\n").unwrap();

        let dir = base.join("src/deep");
        let scss = "@use \"@acme/tokens\" as t;\n@use \"~@acme/tokens/src/mixins\";\n@import \"plain-css\";\n.a { color: t.$brand; @include mixins.pad; }";
        let css = compile_sass(scss, Some(&dir)).unwrap();
        assert!(
            css.contains("#123456"),
            "package sass entry resolved: {css}"
        );
        assert!(css.contains("padding: 4px"), "~pkg/path resolved: {css}");
        assert!(css.contains(".plain"), "style entry resolved: {css}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sass_load_paths_from_config_resolve_bare_imports() {
        let base = std::env::temp_dir().join(format!("oj-css-lp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("styles")).unwrap();
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("styles/_theme.scss"), "$accent: #abc;\n").unwrap();
        let opts = SassOptions {
            load_dir: Some(&base.join("src")),
            additional_data: Some("$pad: 2px;"),
            load_paths: &[base.join("styles")],
            resolve: CssResolve::default(),
        };
        let css = compile_sass_opts(
            "@use \"theme\";\n.a { color: theme.$accent; padding: $pad; }",
            &opts,
        )
        .unwrap();
        assert!(css.contains("#abc") && css.contains("2px"), "{css}");
        assert!(
            compile_sass("@use \"theme\";", Some(&base.join("src"))).is_err(),
            "not on the load path without config"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn strip_sass_import_ext_only_touches_import_specifiers() {
        // extension dropped on @use/@forward/@import so grass takes the bare path
        assert_eq!(
            strip_sass_import_ext("@use \"../css/vars.module.scss\" as *;\n"),
            "@use \"../css/vars.module\" as *;\n"
        );
        assert_eq!(
            strip_sass_import_ext("@forward './theme.scss';\n"),
            "@forward './theme';\n"
        );
        // a plain CSS @import keeps its .css; url(...) is left alone
        assert_eq!(
            strip_sass_import_ext("@import \"reset.css\";\n"),
            "@import \"reset.css\";\n"
        );
        assert_eq!(
            strip_sass_import_ext("@import url(\"x.scss\");\n"),
            "@import url(\"x.scss\");\n"
        );
        // a `.scss` string in a normal declaration must not be rewritten
        assert_eq!(
            strip_sass_import_ext(".a { content: \"file.scss\"; }\n"),
            ".a { content: \"file.scss\"; }\n"
        );
    }

    #[test]
    fn sass_missing_import_still_errors() {
        let base = std::env::temp_dir().join(format!("oj-css-miss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let src = "@use \"./nope.module\" as *;\n.x { color: red; }";
        assert!(
            compile_sass(src, Some(&base)).is_err(),
            "a genuinely missing dotted import must still fail, not resolve to nothing"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sass_resolves_alias_and_root_absolute_imports() {
        let base = std::env::temp_dir().join(format!("oj-css-sass-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("src/styles")).unwrap();
        std::fs::create_dir_all(base.join("src/comp")).unwrap();
        std::fs::write(base.join("src/styles/_vars.scss"), "$brand: #f00;\n").unwrap();
        std::fs::write(
            base.join("src/styles/mixins.scss"),
            "@use \"@/styles/vars\";\n@mixin pad { padding: 4px; color: vars.$brand; }\n",
        )
        .unwrap();
        std::fs::write(base.join("src/styles/theme.module.scss"), "$t: 2px;\n").unwrap();
        let alias = vec![("@".to_string(), "./src".to_string())];
        let opts = SassOptions {
            load_dir: Some(&base.join("src/comp")),
            additional_data: None,
            load_paths: &[],
            resolve: CssResolve {
                root: Some(&base),
                public_dir: None,
                alias: &alias,
                ..CssResolve::default()
            },
        };
        // alias, alias with explicit extension, alias inside an imported file,
        // root-absolute, dotted module through an alias.
        let src = "@use \"@/styles/vars\" as v;\n@use '@/styles/mixins.scss' as m;\n@use \"/src/styles/theme.module\" as t;\n.x { color: v.$brand; margin: t.$t; @include m.pad; }";
        let css = compile_sass_opts(src, &opts).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            css.contains("color: #f00") || css.contains("color: red"),
            "{css}"
        );
        assert!(
            css.contains("margin: 2px") && css.contains("padding: 4px"),
            "{css}"
        );
        // Without the alias the import is unresolvable, as before.
        assert!(compile_sass(src, Some(&base.join("src/comp"))).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
