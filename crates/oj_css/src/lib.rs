// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use lightningcss::css_modules;
use lightningcss::dependencies::{Dependency, DependencyOptions};
use lightningcss::error::{Error as CssError, ParserError};
use lightningcss::printer::PrinterOptions;
use lightningcss::stylesheet::{MinifyOptions, ParserOptions, StyleSheet};
use lightningcss::targets::{Browsers, Targets};

/// Recovered parse errors, printed the way a PostCSS plugin warning would be:
/// the stylesheet still compiles, the developer still learns what was dropped.
fn report_css_warnings(name: &str, warnings: &RwLock<Vec<CssError<ParserError<'_>>>>) {
    let Ok(list) = warnings.read() else {
        return;
    };
    const SHOWN: usize = 5;
    for w in list.iter().take(SHOWN) {
        eprintln!("oj: css warning in {name}: {w}");
    }
    if list.len() > SHOWN {
        eprintln!("oj: css warning in {name}: {} more", list.len() - SHOWN);
    }
}

#[derive(Debug)]
pub struct CssOutput {
    pub css: String,
    pub exports: Option<Vec<(String, String)>>,
}

/// How specifiers inside a stylesheet resolve beyond plain relative paths, the
/// way Vite's CSS resolvers (`createCSSResolvers`, the `url()` rewriter) do:
/// `resolve.alias` pairs apply to `@import`, `@use` and `url()` specifiers, and
/// a root-absolute `/src/x` resolves against the project root, with a file in
/// the public directory taking precedence (`checkPublicFile`).
#[derive(Debug, Clone, Copy)]
pub struct CssResolve<'a> {
    pub root: Option<&'a Path>,
    pub public_dir: Option<&'a Path>,
    /// `(find, replacement)`; a replacement starting with `.` is root-relative.
    pub alias: &'a [(String, String)],
    /// Browser targets lightningcss lowers to (`build.cssTarget`, falling back
    /// to `build.target`), as esbuild-style names (`chrome111`, `safari16.4`);
    /// empty means Vite's `baseline-widely-available` default.
    pub targets: &'a [String],
    /// `build.cssMinify`: whether a build compile minifies (dev never does).
    pub minify: bool,
    /// Vite's `css.modules` options.
    pub modules: &'a CssModulesOptions,
}

static DEFAULT_MODULES: CssModulesOptions = CssModulesOptions {
    locals_convention: None,
    generate_scoped_name: None,
    global_scope: false,
    global_module_paths: Vec::new(),
};

impl Default for CssResolve<'_> {
    fn default() -> Self {
        CssResolve {
            root: None,
            public_dir: None,
            alias: &[],
            targets: &[],
            minify: false,
            modules: &DEFAULT_MODULES,
        }
    }
}

/// Vite's `css.modules` (postcss-modules) options oj applies.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CssModulesOptions {
    /// `localsConvention`: `camelCase`, `camelCaseOnly`, `dashes` or
    /// `dashesOnly`; None keeps class names as written.
    pub locals_convention: Option<String>,
    /// `generateScopedName` as a pattern string (`[name]`, `[local]`, `[hash]`,
    /// `[hash:base64:5]`...); the default is `[name]_[local]_[hash]`.
    pub generate_scoped_name: Option<String>,
    /// `scopeBehaviour: "global"`: module files are compiled unscoped.
    pub global_scope: bool,
    /// `globalModulePaths` regex sources: a module file whose path matches is
    /// compiled unscoped (its export map is empty).
    pub global_module_paths: Vec<String>,
}

/// Owned `CssResolve`, for holders that outlive a borrow (server state, build
/// plugins); `as_ref()` borrows it for a compile.
#[derive(Debug, Default, Clone)]
pub struct CssResolveConfig {
    pub root: PathBuf,
    pub public_dir: PathBuf,
    pub alias: Vec<(String, String)>,
    pub targets: Vec<String>,
    pub minify: bool,
    pub modules: CssModulesOptions,
}

impl CssResolveConfig {
    pub fn as_ref(&self) -> CssResolve<'_> {
        fn set(p: &Path) -> Option<&Path> {
            (!p.as_os_str().is_empty()).then_some(p)
        }
        CssResolve {
            root: set(&self.root),
            public_dir: set(&self.public_dir),
            alias: &self.alias,
            targets: &self.targets,
            minify: self.minify,
            modules: &self.modules,
        }
    }
}

impl CssResolve<'_> {
    /// The specifier after `resolve.alias`, when an alias applies. Matching
    /// follows @rollup/plugin-alias: `find` is the whole specifier or a `find/`
    /// prefix, and only the prefix is replaced. The result is an absolute path
    /// for a path alias, or another bare specifier for a package alias.
    pub fn alias_spec(&self, spec: &str) -> Option<String> {
        for (find, replacement) in self.alias {
            if find.is_empty() {
                continue;
            }
            let rest = if spec == find {
                ""
            } else {
                match spec.strip_prefix(find.as_str()) {
                    Some(rest) if rest.starts_with('/') => rest,
                    _ => continue,
                }
            };
            let target = match (replacement.starts_with('.'), self.root) {
                (true, Some(root)) => {
                    let mut p = root.to_path_buf();
                    for c in Path::new(replacement).components() {
                        match c {
                            std::path::Component::ParentDir => {
                                p.pop();
                            }
                            std::path::Component::Normal(s) => p.push(s),
                            _ => {}
                        }
                    }
                    p.to_string_lossy().into_owned()
                }
                _ => replacement.clone(),
            };
            return Some(format!("{target}{rest}"));
        }
        None
    }

    /// An absolute filesystem path an alias maps `spec` to (a package alias
    /// yields None: it is still a bare specifier).
    pub fn alias_path(&self, spec: &str) -> Option<PathBuf> {
        let aliased = self.alias_spec(spec)?;
        Path::new(&aliased)
            .is_absolute()
            .then(|| PathBuf::from(aliased))
    }

    /// The public-directory file a root-absolute `/x` names, if it exists.
    pub fn public_file(&self, spec: &str) -> Option<PathBuf> {
        let rel = root_absolute_rel(spec)?;
        let p = self.public_dir?.join(rel);
        p.is_file().then_some(p)
    }

    /// The path under the project root a root-absolute `/x` names (existence is
    /// the caller's business, so extension probing can apply).
    pub fn root_path(&self, spec: &str) -> Option<PathBuf> {
        Some(self.root?.join(root_absolute_rel(spec)?))
    }

    /// The dev-server url of `file`: `/rel` inside the root, `/@fs` outside it.
    fn dev_url(&self, file: &Path) -> String {
        match self.root.and_then(|r| file.strip_prefix(r).ok()) {
            Some(rel) => format!("/{}", rel.to_string_lossy().replace('\\', "/")),
            None => format!("/@fs{}", file.display()),
        }
    }
}

/// `/src/x` -> `src/x`; None for anything that is not a root-absolute spec
/// (`//cdn`, `/` alone).
fn root_absolute_rel(spec: &str) -> Option<&str> {
    let rel = spec.strip_prefix('/')?;
    (!rel.is_empty() && !rel.starts_with('/')).then_some(rel)
}

/// The JS module body for a CSS module's class map, shaped like Vite's
/// `dataToEsm(modules, { namedExports: true })`: the whole map is the default
/// export and every class whose name is a legal JS identifier (as
/// @rollup/pluginutils' makeLegalIdentifier judges it) is also a named export,
/// so `import { button } from "./x.module.css"` works.
pub fn css_modules_esm(exports: &[(String, String)]) -> String {
    let mut out = String::new();
    let mut map = serde_json::Map::new();
    for (name, scoped) in exports {
        if is_legal_identifier(name) {
            out.push_str(&format!(
                "export const {name} = {};\n",
                serde_json::Value::String(scoped.clone())
            ));
        }
        map.insert(name.clone(), serde_json::Value::String(scoped.clone()));
    }
    out.push_str(&format!(
        "export default {};\n",
        serde_json::Value::Object(map)
    ));
    out
}

/// `key === makeLegalIdentifier(key)`: identifier characters only, not
/// digit-led, not a reserved word or a global builtin name.
fn is_legal_identifier(name: &str) -> bool {
    const FORBIDDEN: &[&str] = &[
        "break",
        "case",
        "class",
        "catch",
        "const",
        "continue",
        "debugger",
        "default",
        "delete",
        "do",
        "else",
        "export",
        "extends",
        "finally",
        "for",
        "function",
        "if",
        "import",
        "in",
        "instanceof",
        "let",
        "new",
        "return",
        "super",
        "switch",
        "this",
        "throw",
        "try",
        "typeof",
        "var",
        "void",
        "while",
        "with",
        "yield",
        "enum",
        "await",
        "implements",
        "package",
        "protected",
        "static",
        "interface",
        "private",
        "public",
        "arguments",
        "Infinity",
        "NaN",
        "undefined",
        "null",
        "true",
        "false",
        "eval",
        "uneval",
        "isFinite",
        "isNaN",
        "parseFloat",
        "parseInt",
        "decodeURI",
        "decodeURIComponent",
        "encodeURI",
        "encodeURIComponent",
        "escape",
        "unescape",
        "Object",
        "Function",
        "Boolean",
        "Symbol",
        "Error",
        "EvalError",
        "InternalError",
        "RangeError",
        "ReferenceError",
        "SyntaxError",
        "TypeError",
        "URIError",
        "Number",
        "Math",
        "Date",
        "String",
        "RegExp",
        "Array",
        "Int8Array",
        "Uint8Array",
        "Uint8ClampedArray",
        "Int16Array",
        "Uint16Array",
        "Int32Array",
        "Uint32Array",
        "Float32Array",
        "Float64Array",
        "Map",
        "Set",
        "WeakMap",
        "WeakSet",
        "SIMD",
        "ArrayBuffer",
        "DataView",
        "JSON",
        "Promise",
        "Generator",
        "GeneratorFunction",
        "Reflect",
        "Proxy",
        "Intl",
    ];
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let ident_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$';
    ident_char(first)
        && !first.is_ascii_digit()
        && chars.all(ident_char)
        && !FORBIDDEN.contains(&name)
}

pub fn is_css_module(url: &str) -> bool {
    let path = url.split('?').next().unwrap_or(url);
    path.rsplit('/')
        .next()
        .is_some_and(|f| f.contains(".module."))
}

pub fn is_sass(url: &str) -> bool {
    let f = url.split('?').next().unwrap_or(url);
    f.ends_with(".scss") || f.ends_with(".sass")
}

fn with_ext(p: &Path, ext: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

// A dotted-basename stylesheet (`x.module.scss`) is presented to grass as a
// directory `x.module/` whose index is that file (see index_target). A relative
// import made from inside such a file then carries that phantom directory
// segment (grass resolves `@use 'sibling'` against `.../x.module/`). Collapse
// any intermediate segment `seg` for which `<...>/seg.scss` (or `.sass`) is a
// real file, since `seg` is really that file, not a directory. Returns a new
// path only when something was collapsed.
fn collapse_phantom_dirs(p: &Path) -> Option<PathBuf> {
    let comps: Vec<_> = p.components().collect();
    let mut out = PathBuf::new();
    let mut changed = false;
    for (i, comp) in comps.iter().enumerate() {
        let candidate = out.join(comp);
        let is_last = i + 1 == comps.len();
        if !is_last
            && (with_ext(&candidate, "scss").is_file() || with_ext(&candidate, "sass").is_file())
        {
            // `candidate` names a dotted stylesheet file, so it is a phantom
            // directory: drop it and resolve siblings against its real parent.
            changed = true;
            continue;
        }
        out = candidate;
    }
    changed.then_some(out)
}

// If `p.scss`/`p.sass` exists as a real file, return it. Used to treat a
// dotted-basename stylesheet as resolvable.
fn dotted_stylesheet(p: &Path) -> Option<PathBuf> {
    for ext in ["scss", "sass"] {
        let c = with_ext(p, ext);
        if c.is_file() {
            return Some(c);
        }
    }
    if let Some(collapsed) = collapse_phantom_dirs(p) {
        for ext in ["scss", "sass"] {
            let c = with_ext(&collapsed, ext);
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

// Map a grass directory-index probe `.../X/index.scss` (or _index/.sass) back
// to a real `.../X.scss` file when `X` is a dotted-basename stylesheet. The
// mapping is extension-faithful: an `index.sass` probe only matches a real
// `.sass` file, so scss content is never parsed as the indented syntax.
fn index_target(p: &Path) -> Option<PathBuf> {
    let name = p.file_name()?.to_str()?;
    let ext = match name {
        "index.scss" | "_index.scss" => "scss",
        "index.sass" | "_index.sass" => "sass",
        _ => return None,
    };
    let parent = p.parent()?;
    let c = with_ext(parent, ext);
    if c.is_file() {
        return Some(c);
    }
    // The dotted-basename file may sit behind a phantom directory carried in
    // from a relative import (see collapse_phantom_dirs).
    let collapsed = collapse_phantom_dirs(parent)?;
    let c = with_ext(&collapsed, ext);
    c.is_file().then_some(c)
}

// grass 0.13 treats a trailing `.segment` in an `@use`/`@import` path as an
// extension, so `@use "../css/variables.module"` never probes
// `variables.module.scss` and fails on the CSS-modules `.module.scss`
// convention. grass does probe the basename as a directory, so present a real
// `name.segment.scss` file as a directory whose index is that file. This makes
// resolution match dart-sass (what Vite uses), which treats the whole basename
// as the stylesheet name.
#[derive(Debug)]
struct DottedFs {
    /// Alias / root-absolute rewriting for the import lines of every file read
    /// through the fs, so nested `@use "@/x"` resolves like the entry's.
    resolve: CssResolveConfig,
}

// grass resolves a directory import `@use "pkg"` by probing `pkg/index.scss` and
// `pkg/_index.scss` (or `.sass`). When `pkg` is an npm package reached through
// a node_modules load path, its package.json names the stylesheet instead:
// Vite's sass importer resolves with mainFields `["sass", "style"]` (then
// `main`). Map the probe to that file. Syntax follows the probed extension
// (grass parses by it), so an `index.sass` probe only accepts a `.sass` entry
// and an `index.scss` probe accepts `.scss` or plain `.css`.
fn package_entry(probe: &Path) -> Option<PathBuf> {
    let name = probe.file_name()?.to_str()?;
    let want_sass = match name {
        "index.scss" | "_index.scss" => false,
        "index.sass" | "_index.sass" => true,
        _ => return None,
    };
    let dir = probe.parent()?;
    let pkg = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let pkg: serde_json::Value = serde_json::from_str(&pkg).ok()?;
    let accepts = |p: &Path| {
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        p.is_file()
            && if want_sass {
                ext == "sass"
            } else {
                ext == "scss" || ext == "css"
            }
    };
    for field in ["sass", "style", "main"] {
        let Some(rel) = pkg.get(field).and_then(|v| v.as_str()) else {
            continue;
        };
        let p = dir.join(rel);
        if accepts(&p) {
            return Some(p);
        }
        let exts: &[&str] = if want_sass {
            &["sass"]
        } else {
            &["scss", "css"]
        };
        for ext in exts {
            let c = with_ext(&p, ext);
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

// The real file behind a grass probe that is not itself a file: a dotted-basename
// stylesheet's phantom index, or an npm package's `sass`/`style`/`main` entry.
fn probe_target(p: &Path) -> Option<PathBuf> {
    index_target(p).or_else(|| package_entry(p))
}

impl grass::Fs for DottedFs {
    fn is_dir(&self, p: &Path) -> bool {
        p.is_dir() || dotted_stylesheet(p).is_some()
    }
    fn is_file(&self, p: &Path) -> bool {
        p.is_file() || probe_target(p).is_some()
    }
    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        let bytes = if p.is_file() {
            std::fs::read(p)?
        } else {
            match probe_target(p) {
                Some(real) => std::fs::read(real)?,
                None => std::fs::read(p)?,
            }
        };
        // Nested `@use`/`@forward`/`@import` in read files need the same
        // extension stripping the entry gets, so a dotted specifier like
        // `@use 'colors.module.scss'` inside an imported stylesheet resolves
        // through DottedFs instead of grass probing the raw dotted+ext path.
        match String::from_utf8(bytes) {
            Ok(text) => Ok(prepare_sass_imports(&text, &self.resolve.as_ref()).into_bytes()),
            Err(e) => Ok(e.into_bytes()),
        }
    }
}

/// Whether `base` names a Sass stylesheet the way dart-sass probes it: the file
/// itself, `.scss`/`.sass`/`.css` added, a `_partial`, or a directory index.
fn sass_file_exists(base: &Path) -> bool {
    if base.is_file() {
        return true;
    }
    let Some(name) = base.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let parent = base.parent().unwrap_or(Path::new("."));
    for ext in ["scss", "sass", "css"] {
        if with_ext(base, ext).is_file() || parent.join(format!("_{name}.{ext}")).is_file() {
            return true;
        }
    }
    ["index.scss", "_index.scss", "index.sass", "_index.sass"]
        .iter()
        .any(|i| base.join(i).is_file())
}

/// Vite's sass importer resolves through `resolve.alias` and root-absolute
/// paths (its idResolver); grass knows neither, so rewrite such a specifier to
/// the absolute file path before grass sees it. A root-absolute spec is only
/// rewritten when a stylesheet exists under the root (otherwise it may be a
/// real absolute path).
fn rewrite_sass_spec(spec: &str, resolve: &CssResolve<'_>) -> Option<String> {
    if let Some(p) = resolve.alias_path(spec) {
        return Some(p.to_string_lossy().into_owned());
    }
    let under_root = resolve.root_path(spec)?;
    sass_file_exists(&under_root).then(|| under_root.to_string_lossy().into_owned())
}

/// Apply `rewrite_sass_spec` to every quoted string of an import line.
fn rewrite_sass_line(line: &str, resolve: &CssResolve<'_>) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find(['"', '\'']) {
        let quote = rest.as_bytes()[open] as char;
        let Some(close) = rest[open + 1..].find(quote) else {
            break;
        };
        let inner = &rest[open + 1..open + 1 + close];
        out.push_str(&rest[..=open]);
        match rewrite_sass_spec(inner, resolve) {
            Some(path) => out.push_str(&path),
            None => out.push_str(inner),
        }
        out.push(quote);
        rest = &rest[open + 1 + close + 1..];
    }
    out.push_str(rest);
    out
}

// grass also mishandles an explicit `.scss`/`.sass` extension on a dotted
// basename (`@use "../css/vars.module.scss"`), probing only CWD-relative and
// never through the load paths. Drop the extension on `@use`/`@forward`/
// `@import` specifiers so every import takes the bare-name path (which DottedFs
// resolves); Sass treats `@use "x"` and `@use "x.scss"` identically. `.css` is
// left alone (it stays a plain CSS import), as are `url(...)` lines.
#[cfg(test)]
fn strip_sass_import_ext(source: &str) -> String {
    prepare_sass_imports(source, &CssResolve::default())
}

fn prepare_sass_imports(source: &str, resolve: &CssResolve<'_>) -> String {
    let mut out = String::with_capacity(source.len());
    let has_resolve = resolve.root.is_some() || !resolve.alias.is_empty();
    for line in source.split_inclusive('\n') {
        let t = line.trim_start();
        let is_import =
            t.starts_with("@use") || t.starts_with("@forward") || t.starts_with("@import");
        if is_import && !line.contains("url(") {
            // `~pkg/...` is the webpack-era spelling of a node_modules import that
            // Vite's sass importer still accepts; the load paths cover it bare.
            let stripped = line
                .replace(".scss\"", "\"")
                .replace(".scss'", "'")
                .replace(".sass\"", "\"")
                .replace(".sass'", "'");
            // Aliases apply before the `~` strip so a configured `~` alias wins.
            let aliased = if has_resolve {
                rewrite_sass_line(&stripped, resolve)
            } else {
                stripped
            };
            out.push_str(&aliased.replace("\"~", "\"").replace("'~", "'"));
        } else {
            out.push_str(line);
        }
    }
    out
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
            load_paths: &[],
            resolve: CssResolve::default(),
        },
    )
}

/// How a Sass stylesheet is compiled: the importing file's directory, the
/// configured `css.preprocessorOptions.scss.loadPaths`/`includePaths`, and the
/// `node_modules` directories above it (so `@use "bootstrap/scss/bootstrap"` and
/// `@use "pkg"` resolve as in Vite), plus `additionalData` prepended, and
/// `resolve.alias` / root-absolute specifiers as Vite's sass importer resolves them.
#[derive(Debug, Default, Clone, Copy)]
pub struct SassOptions<'a> {
    pub load_dir: Option<&'a Path>,
    pub additional_data: Option<&'a str>,
    pub load_paths: &'a [PathBuf],
    pub resolve: CssResolve<'a>,
}

/// Every `node_modules` directory from `dir` up to the filesystem root, nearest
/// first: Sass' node resolution for bare `@use`/`@import` specifiers.
pub fn node_modules_load_paths(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if d.file_name().is_some_and(|n| n == "node_modules") {
            cur = d.parent();
            continue;
        }
        let nm = d.join("node_modules");
        if nm.is_dir() {
            out.push(nm);
        }
        cur = d.parent();
    }
    out
}

pub fn compile_sass_opts(source: &str, opts: &SassOptions<'_>) -> Result<String, String> {
    let fs = DottedFs {
        resolve: CssResolveConfig {
            root: opts.resolve.root.map(Path::to_path_buf).unwrap_or_default(),
            public_dir: opts
                .resolve
                .public_dir
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            alias: opts.resolve.alias.to_vec(),
            targets: opts.resolve.targets.to_vec(),
            minify: opts.resolve.minify,
            modules: opts.resolve.modules.clone(),
        },
    };
    let mut options = grass::Options::default().fs(&fs);
    if let Some(dir) = opts.load_dir {
        options = options.load_path(dir);
    }
    for p in opts.load_paths {
        options = options.load_path(p);
    }
    if let Some(dir) = opts.load_dir {
        for nm in node_modules_load_paths(dir) {
            options = options.load_path(nm);
        }
    }
    let additional_data = opts.additional_data;
    let stripped = prepare_sass_imports(source, &opts.resolve);
    let source = match additional_data {
        Some(data) if !data.is_empty() => format!("{data}\n{stripped}"),
        _ => stripped,
    };
    grass::from_string(source, &options).map_err(|e| format!("sass error: {e}"))
}

/// Vite 8's `baseline-widely-available` target list (its `build.target` and
/// so `build.cssTarget` default).
const BASELINE_TARGETS: &[&str] = &[
    "chrome111",
    "edge111",
    "firefox114",
    "safari16.4",
    "ios16.4",
];

/// esbuild-style target names as lightningcss browser targets, as Vite's
/// `convertTargets` (css.ts) maps them: `chrome`, `edge`, `firefox`, `ie`,
/// `ios` (-> ios_saf), `opera`, `safari`; the lowest version per browser wins;
/// `es20xx`/`esnext` and unknown names carry no browser and are skipped. No
/// browser at all falls back to the baseline.
pub fn browser_targets(list: &[String]) -> Targets {
    let mut b = Browsers::default();
    let mut any = false;
    for entry in list {
        let split = entry
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(entry.len());
        let (name, version) = entry.split_at(split);
        let mut parts = version.split('.').map(|v| v.parse::<u32>().ok());
        let (Some(Some(major)), minor) = (parts.next(), parts.next().flatten().unwrap_or(0)) else {
            continue;
        };
        let v = (major << 16) | (minor << 8);
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
    if !any {
        if list.iter().any(|t| t == "modules") {
            // Vite's legacy `modules` preset.
            return browser_targets(
                &["chrome87", "edge88", "firefox78", "safari14"].map(String::from),
            );
        }
        return default_targets();
    }
    Targets::from(b)
}

fn default_targets() -> Targets {
    browser_targets(
        &BASELINE_TARGETS
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
    )
}

pub fn compile_css(url: &str, source: &str, minify: bool) -> Result<CssOutput, String> {
    compile_css_impl(url, source, minify, false, false, &CssResolve::default())
}

/// The build compile with the app's settings: minified when `resolve.minify`
/// (`build.cssMinify`), lowered to `resolve.targets` (`build.cssTarget`).
pub fn compile_css_with(
    url: &str,
    source: &str,
    resolve: &CssResolve<'_>,
) -> Result<CssOutput, String> {
    compile_css_impl(url, source, resolve.minify, false, false, resolve)
}

pub fn compile_css_rebased(url: &str, source: &str, minify: bool) -> Result<CssOutput, String> {
    compile_css_impl(url, source, minify, true, false, &CssResolve::default())
}

/// `compile_css_rebased` plus an inline source map (`css.devSourcemap`): the
/// served CSS ends with a `sourceMappingURL` data URL mapping back to `url`,
/// with the (preprocessed) source embedded, so devtools show the stylesheet's
/// rules at their source lines.
pub fn compile_css_rebased_with_map(
    url: &str,
    source: &str,
    minify: bool,
) -> Result<CssOutput, String> {
    compile_css_impl(url, source, minify, true, true, &CssResolve::default())
}

/// The dev-server compile: `url()` / `@import` paths rebased to server-absolute
/// urls, an aliased one (`@/img.png`) to the url of the file it names, as
/// Vite's url rewriter does in dev; `source_map` adds the inline map.
pub fn compile_css_dev(
    url: &str,
    source: &str,
    source_map: bool,
    resolve: &CssResolve<'_>,
) -> Result<CssOutput, String> {
    compile_css_impl(url, source, false, true, source_map, resolve)
}

fn compile_css_impl(
    url: &str,
    source: &str,
    minify: bool,
    rebase: bool,
    source_map: bool,
    resolve: &CssResolve<'_>,
) -> Result<CssOutput, String> {
    compile_css_depth(url, source, minify, rebase, source_map, resolve, 0)
}

/// Whether a `.module.css` file is scoped: not under `scopeBehaviour: "global"`
/// and not matched by a `globalModulePaths` regex (postcss-modules then runs
/// it in global mode, exporting nothing).
fn module_is_scoped(url: &str, resolve: &CssResolve<'_>) -> bool {
    if resolve.modules.global_scope {
        return false;
    }
    if resolve.modules.global_module_paths.is_empty() {
        return true;
    }
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let abs = module_file_path(url, resolve).map(|p| p.to_string_lossy().into_owned());
    !resolve.modules.global_module_paths.iter().any(|src| {
        regex::Regex::new(src)
            .is_ok_and(|re| re.is_match(path) || abs.as_deref().is_some_and(|a| re.is_match(a)))
    })
}

/// The scoped-name pattern for CSS modules: `generateScopedName` translated
/// from postcss-modules' interpolateName tokens (`[hash:base64:5]` -> `[hash]`,
/// `[contenthash]` -> `[content-hash]`; `[path]`/`[folder]`/`[ext]` carry no
/// equivalent and are dropped), or oj's default.
fn scoped_name_pattern(modules: &CssModulesOptions) -> css_modules::Pattern {
    const DEFAULT: &str = "[name]_[local]_[hash]";
    let Some(raw) = modules
        .generate_scoped_name
        .as_deref()
        .filter(|s| !s.is_empty())
    else {
        return css_modules::Pattern::parse(DEFAULT).expect("static pattern");
    };
    let mut out = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find('[') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find(']') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let token = &rest[start + 1..start + end];
        let lower = token.to_ascii_lowercase();
        if lower.starts_with("contenthash") || lower.starts_with("content-hash") {
            out.push_str("[content-hash]");
        } else if lower.starts_with("hash") {
            out.push_str("[hash]");
        } else if lower == "name" || lower == "local" {
            out.push_str(&format!("[{lower}]"));
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    let parsed = if out.contains("[local]") {
        css_modules::Pattern::parse(&out).map_err(|e| format!("{e:?}"))
    } else {
        Err("no [local] placeholder".to_string())
    };
    match parsed {
        Ok(p) => p,
        Err(e) => {
            eprintln!("oj: css.modules.generateScopedName {raw:?} is not a supported pattern ({e}); using {DEFAULT}");
            css_modules::Pattern::parse(DEFAULT).expect("static pattern")
        }
    }
}

/// The file a compile `url` names on disk: an absolute path as-is, a
/// root-relative url under `resolve.root`.
fn module_file_path(url: &str, resolve: &CssResolve<'_>) -> Option<PathBuf> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let p = Path::new(path);
    if p.is_absolute() && p.is_file() {
        return Some(p.to_path_buf());
    }
    let root = resolve.root?;
    let rel = path.trim_start_matches('/');
    let joined = root.join(rel);
    joined.is_file().then_some(joined)
}

/// lodash `camelCase`, as postcss-modules' `camelCase` convention applies it:
/// words split on non-alphanumerics and lower-to-upper transitions, first
/// word lowercased, the rest capitalized.
fn camel_case(s: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.push(c);
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    let mut out = String::new();
    for (i, w) in words.iter().enumerate() {
        let lower = w.to_lowercase();
        if i == 0 {
            out.push_str(&lower);
        } else {
            let mut chars = lower.chars();
            if let Some(first) = chars.next() {
                out.extend(first.to_uppercase());
                out.push_str(chars.as_str());
            }
        }
    }
    out
}

/// postcss-modules' `dashesCamelCase`: only `-x` runs become `X`.
fn dashes_camel_case(s: &str) -> String {
    let mut out = String::new();
    let mut upper_next = false;
    for c in s.chars() {
        if c == '-' {
            upper_next = true;
            continue;
        }
        if upper_next {
            out.extend(c.to_uppercase());
            upper_next = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Apply `localsConvention` to an export map (postcss-modules): `camelCase` /
/// `dashes` add the converted key next to the original, the `*Only` forms
/// replace it.
fn apply_locals_convention(
    pairs: Vec<(String, String)>,
    convention: Option<&str>,
) -> Vec<(String, String)> {
    let (convert, only): (fn(&str) -> String, bool) = match convention {
        Some("camelCase") => (camel_case, false),
        Some("camelCaseOnly") => (camel_case, true),
        Some("dashes") => (dashes_camel_case, false),
        Some("dashesOnly") => (dashes_camel_case, true),
        _ => return pairs,
    };
    let mut out: Vec<(String, String)> = Vec::with_capacity(pairs.len() * 2);
    for (name, value) in pairs {
        let converted = convert(&name);
        if !only && converted != name {
            out.push((name, value.clone()));
        }
        if !out.iter().any(|(k, _)| *k == converted) {
            out.push((converted, value));
        }
    }
    out.sort();
    out
}

/// The export values of a CSS module with `composes` resolved the way
/// postcss-modules exports them: the scoped name followed by every composed
/// class (locals transitively, globals as written, and `from "./other.css"`
/// dependencies compiled from their own module file).
fn expand_composes(
    map: css_modules::CssModuleExports,
    url: &str,
    resolve: &CssResolve<'_>,
    depth: u8,
) -> Vec<(String, String)> {
    use css_modules::CssModuleReference as R;
    fn value_of(
        map: &css_modules::CssModuleExports,
        export: &css_modules::CssModuleExport,
        url: &str,
        resolve: &CssResolve<'_>,
        depth: u8,
        seen: &mut Vec<String>,
    ) -> String {
        let mut value = export.name.clone();
        for r in &export.composes {
            let part = match r {
                R::Local { name } => {
                    // Composing a local class pulls in that class' own composes.
                    match map.values().find(|e| e.name == *name) {
                        Some(inner) if !seen.contains(name) => {
                            seen.push(name.clone());
                            let v = value_of(map, inner, url, resolve, depth, seen);
                            seen.pop();
                            v
                        }
                        _ => name.clone(),
                    }
                }
                R::Global { name } => name.clone(),
                R::Dependency { name, specifier } => {
                    match dependency_export(specifier, name, url, resolve, depth) {
                        Some(v) => v,
                        None => {
                            eprintln!("oj: css module {url}: cannot resolve `composes: {name} from {specifier:?}`");
                            continue;
                        }
                    }
                }
            };
            if !part.is_empty() {
                value.push(' ');
                value.push_str(&part);
            }
        }
        value
    }
    let mut pairs: Vec<(String, String)> = map
        .iter()
        .map(|(name, export)| {
            (
                name.clone(),
                value_of(&map, export, url, resolve, depth, &mut Vec::new()),
            )
        })
        .collect();
    pairs.sort();
    pairs
}

/// `composes: name from "spec"`: the export `name` of the module file `spec`
/// names (relative to the composing file, root-absolute, or aliased), compiled
/// with the same settings.
fn dependency_export(
    spec: &str,
    name: &str,
    url: &str,
    resolve: &CssResolve<'_>,
    depth: u8,
) -> Option<String> {
    if depth > 8 {
        return None;
    }
    let file = module_file_path(url, resolve)?;
    let dir = file.parent()?;
    let dep = if let Some(p) = resolve.alias_path(spec) {
        p
    } else if let Some(rest) = spec.strip_prefix('/') {
        resolve.root?.join(rest)
    } else {
        dir.join(spec)
    };
    // Lexical normalization only: the url (and so the `[hash]`) must be the
    // same root-relative spelling the file gets when imported directly.
    let mut dep_norm = PathBuf::new();
    for c in dep.components() {
        match c {
            std::path::Component::ParentDir => {
                dep_norm.pop();
            }
            std::path::Component::CurDir => {}
            other => dep_norm.push(other),
        }
    }
    let dep = dep_norm;
    let mut source = std::fs::read_to_string(&dep).ok()?;
    if is_sass(&dep.to_string_lossy()) {
        source = compile_sass(&source, dep.parent()).ok()?;
    }
    let dep_url = match resolve.root.and_then(|r| dep.strip_prefix(r).ok()) {
        Some(rel) => format!("/{}", rel.display()),
        None => dep.to_string_lossy().into_owned(),
    };
    let out = compile_css_depth(&dep_url, &source, false, false, false, resolve, depth + 1).ok()?;
    out.exports?
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v)
}

fn compile_css_depth(
    url: &str,
    source: &str,
    minify: bool,
    rebase: bool,
    source_map: bool,
    resolve: &CssResolve<'_>,
    depth: u8,
) -> Result<CssOutput, String> {
    let is_module_file = is_css_module(url);
    let is_module = is_module_file && module_is_scoped(url, resolve);
    let warnings = Arc::new(RwLock::new(Vec::new()));
    let options = ParserOptions {
        filename: url.to_string(),
        css_modules: is_module.then(|| css_modules::Config {
            pattern: scoped_name_pattern(resolve.modules),
            ..css_modules::Config::default()
        }),
        // Vite's default pipeline (postcss) never rejects a stylesheet over a
        // legacy hack (`*zoom: 1`, `_height`, IE `filter: progid:...`) or a
        // stray invalid rule; lightningcss does unless it may recover, in which
        // case the offending declaration/rule is dropped and reported as a
        // warning instead of failing the whole file.
        error_recovery: true,
        warnings: Some(Arc::clone(&warnings)),
        ..ParserOptions::default()
    };

    let mut stylesheet = StyleSheet::parse(source, options)
        .map_err(|err| format!("css parse error in {url}: {err}"))?;
    report_css_warnings(url, &warnings);

    let targets = browser_targets(resolve.targets);
    stylesheet
        .minify(MinifyOptions {
            targets,
            ..MinifyOptions::default()
        })
        .map_err(|err| format!("css transform error in {url}: {err}"))?;

    let base = if rebase { css_base_dir(url) } else { None };
    let mut sm = parcel_sourcemap::SourceMap::new("");
    if source_map {
        let idx = sm.add_source(url);
        let _ = sm.set_source_content(idx as usize, source);
    }
    let result = stylesheet
        .to_css(PrinterOptions {
            minify,
            targets,
            analyze_dependencies: base.as_ref().map(|_| DependencyOptions::default()),
            source_map: source_map.then_some(&mut sm),
            ..PrinterOptions::default()
        })
        .map_err(|err| format!("css print error in {url}: {err}"))?;

    let mut css = result.code;
    if let (Some(base), Some(deps)) = (base, result.dependencies) {
        let mut pairs = Vec::with_capacity(deps.len());
        for dep in deps {
            let (placeholder, orig) = match dep {
                Dependency::Url(u) => (u.placeholder, u.url),
                Dependency::Import(i) => (i.placeholder, i.url),
            };
            let replacement = dev_url_of(&orig, &base, resolve);
            pairs.push((placeholder, replacement));
        }
        css = substitute_placeholders(css, &pairs);
    }
    if source_map {
        // Sources are stored root-relative (`src/app.css`); `sourceRoot: "/"`
        // makes devtools resolve them to the served url (`/src/app.css`).
        let json = sm
            .to_json(Some("/"))
            .map_err(|err| format!("css sourcemap error in {url}: {err}"))?;
        css.push_str("\n/*# sourceMappingURL=data:application/json;base64,");
        css.push_str(&base64(json.as_bytes()));
        css.push_str(" */\n");
    }

    let exports = match result.exports {
        Some(map) => Some(apply_locals_convention(
            expand_composes(map, url, resolve, depth),
            resolve.modules.locals_convention.as_deref(),
        )),
        // A module file compiled in global mode still is a module to its
        // importer: an empty class map, as postcss-modules exports for it.
        None if is_module_file => Some(Vec::new()),
        None => None,
    };

    Ok(CssOutput { css, exports })
}

/// Inline plain `@import` rules the way Vite's postcss-import step does, so a
/// stylesheet served in dev or concatenated into a build chunk carries its
/// imported rules instead of an `@import` the browser would resolve against the
/// wrong URL (or a bare specifier it cannot resolve at all). Relative imports
/// resolve against the importing file; bare ones (`normalize.css`, `~pkg/x`)
/// through the `node_modules` directories above it via the package's `style`
/// or `main` entry. Imports with a media query, `layer(...)` or `supports(...)`
/// and external urls are left as written. Every inlined file's relative `url()`
/// and remaining `@import` paths are rebased to `file`'s directory.
pub fn inline_imports(source: &str, file: &Path) -> Result<String, String> {
    inline_imports_with(source, file, &CssResolve::default())
}

/// `inline_imports` with `resolve.alias` and root-absolute specifiers resolved
/// like Vite's postcss-import resolver (public dir first, then the root).
pub fn inline_imports_with(
    source: &str,
    file: &Path,
    resolve: &CssResolve<'_>,
) -> Result<String, String> {
    let mut stack = vec![file.to_path_buf()];
    let out = inline_imports_depth(source, file, &mut stack, resolve)?;
    Ok(if out.contains("@import") {
        hoist_imports(&out)
    } else {
        out
    })
}

/// Move every statement-level `@import ...;` that survived inlining (external
/// urls, media-query imports, unresolvable specifiers) to the top, after a
/// leading `@charset`, since CSS requires imports to precede all other rules and
/// an inlined file's rules may now sit above them.
fn hoist_imports(css: &str) -> String {
    let mut imports: Vec<&str> = Vec::new();
    let mut body = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(pos) = rest.find("@import") {
        let (before, at) = rest.split_at(pos);
        let statement_start = before
            .trim_end()
            .chars()
            .next_back()
            .is_none_or(|c| matches!(c, ';' | '}' | '{'));
        let end = if statement_start { at.find(';') } else { None };
        let Some(end) = end else {
            body.push_str(before);
            body.push_str("@import");
            rest = &at[7..];
            continue;
        };
        body.push_str(before);
        imports.push(&at[..=end]);
        rest = &at[end + 1..];
    }
    body.push_str(rest);
    if imports.is_empty() {
        return body;
    }
    let mut out = String::with_capacity(css.len());
    let mut body_rest = body.as_str();
    if let Some(after) = body_rest.trim_start().strip_prefix("@charset") {
        if let Some(end) = after.find(';') {
            let stmt_end = body_rest.len() - after.len() + end + 1;
            out.push_str(&body_rest[..stmt_end]);
            out.push('\n');
            body_rest = &body_rest[stmt_end..];
        }
    }
    for imp in imports {
        out.push_str(imp.trim_start());
        out.push('\n');
    }
    out.push_str(body_rest);
    out
}

fn inline_imports_depth(
    source: &str,
    file: &Path,
    stack: &mut Vec<PathBuf>,
    resolve: &CssResolve<'_>,
) -> Result<String, String> {
    if !source.contains("@import") || stack.len() > 32 {
        return Ok(source.to_string());
    }
    let dir = file.parent().unwrap_or(Path::new("."));
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(pos) = rest.find("@import") {
        let (before, at) = rest.split_at(pos);
        // Only a statement-level `@import` (preceded by nothing, `;`, `}` or `{`
        // modulo whitespace) counts; anything else is inside a string or comment.
        let statement_start = before
            .trim_end()
            .chars()
            .next_back()
            .is_none_or(|c| matches!(c, ';' | '}' | '{' | '/'));
        let parsed = if statement_start {
            parse_plain_import(&at[7..])
        } else {
            None
        };
        let Some((spec, consumed, media)) = parsed else {
            out.push_str(before);
            out.push_str("@import");
            rest = &at[7..];
            continue;
        };
        let stmt_len = 7 + consumed;
        let resolved = resolve_css_import_with(&spec, dir, resolve);
        let Some(target) = resolved.filter(|t| !stack.iter().any(|s| s == t)) else {
            out.push_str(before);
            out.push_str(&at[..stmt_len]);
            rest = &at[stmt_len..];
            continue;
        };
        let child = std::fs::read_to_string(&target)
            .map_err(|e| format!("cannot read @import {spec} ({}): {e}", target.display()))?;
        stack.push(target.clone());
        let child = inline_imports_depth(&child, &target, stack, resolve)?;
        stack.pop();
        let child_dir = target.parent().unwrap_or(Path::new("."));
        let rebased = rebase_to_dir(&child, &target, child_dir, dir, resolve)?;
        out.push_str(before);
        match media {
            // `@import "x" print;` becomes `@media print { ... }` (postcss-import).
            Some(media) => {
                out.push_str("@media ");
                out.push_str(&media);
                out.push_str(" {\n");
                out.push_str(&rebased);
                out.push_str("\n}\n");
            }
            None => {
                out.push_str(&rebased);
                out.push('\n');
            }
        }
        rest = &at[stmt_len..];
    }
    out.push_str(rest);
    Ok(out)
}

/// `"x"`, `'x'`, `url(x)`, `url("x")`, optionally followed by a media query,
/// then `;`. Returns the specifier, how many bytes of `after` the statement
/// (through `;`) spans, and the media query if any. `layer(...)`/`supports(...)`
/// imports are not inlined (None).
fn parse_plain_import(after: &str) -> Option<(String, usize, Option<String>)> {
    let trimmed = after.trim_start();
    let ws = after.len() - trimmed.len();
    let (spec, used) = if let Some(inner) = trimmed.strip_prefix("url(") {
        let close = inner.find(')')?;
        let raw = inner[..close]
            .trim()
            .trim_matches(|c| c == '"' || c == '\'');
        (raw.to_string(), 4 + close + 1)
    } else {
        let quote = trimmed.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let close = trimmed[1..].find(quote)?;
        (trimmed[1..1 + close].to_string(), 1 + close + 1)
    };
    let tail = &trimmed[used..];
    let semi = tail.find(';')?;
    if tail[..semi].contains(['{', '}']) {
        return None;
    }
    let cond = tail[..semi].trim();
    // `layer(...)` / `supports(...)` imports keep their rule for the browser.
    if cond.contains("layer(") || cond.contains("supports(") || cond.starts_with("layer") {
        return None;
    }
    if spec.is_empty()
        || spec.starts_with("data:")
        || spec.starts_with("//")
        || spec.contains("://")
    {
        return None;
    }
    let media = (!cond.is_empty()).then(|| cond.to_string());
    Some((spec, ws + used + semi + 1, media))
}

fn first_css_file(base: &Path) -> Option<PathBuf> {
    css_file_candidates(base).into_iter().find(|c| c.is_file())
}

fn css_file_candidates(base: &Path) -> Vec<PathBuf> {
    let mut v = vec![base.to_path_buf()];
    if base.extension().is_none() {
        v.push(with_ext(base, "css"));
    }
    v.push(base.join("index.css"));
    v
}

/// postcss-import's resolution order: relative to the importer, then a
/// node_modules package (`style`/`main` entry for a bare package, a file inside
/// it otherwise), `~` prefix accepted.
pub fn resolve_css_import(spec: &str, dir: &Path) -> Option<PathBuf> {
    resolve_css_import_with(spec, dir, &CssResolve::default())
}

/// `resolve_css_import` preceded by Vite's alias and root-absolute steps: an
/// alias rewrites the specifier first (a path alias resolves there, a package
/// alias continues as a bare specifier); a root-absolute `/x` is the public file
/// when one exists, else a file under the root, else a real absolute path.
pub fn resolve_css_import_with(
    spec: &str,
    dir: &Path,
    resolve: &CssResolve<'_>,
) -> Option<PathBuf> {
    let spec = spec.split(['?', '#']).next().unwrap_or(spec);
    let aliased = resolve.alias_spec(spec);
    let spec = match &aliased {
        Some(a) if Path::new(a).is_absolute() => return first_css_file(Path::new(a)),
        Some(a) => a.as_str(),
        None if spec.starts_with('/') => {
            if let Some(public) = resolve.public_file(spec) {
                return Some(public);
            }
            if let Some(under_root) = resolve.root_path(spec).and_then(|p| first_css_file(&p)) {
                return Some(under_root);
            }
            return first_css_file(Path::new(spec));
        }
        None => spec,
    };
    let spec = spec.strip_prefix('~').unwrap_or(spec);
    if spec.is_empty() {
        return None;
    }
    for c in css_file_candidates(&dir.join(spec)) {
        if c.is_file() {
            return Some(c);
        }
    }
    if spec.starts_with("./") || spec.starts_with("../") {
        return None;
    }
    let (pkg, rest) = split_package_specifier(spec)?;
    for nm in node_modules_load_paths(dir) {
        let pkg_dir = nm.join(pkg);
        if !pkg_dir.is_dir() {
            continue;
        }
        if rest.is_empty() {
            if let Ok(text) = std::fs::read_to_string(pkg_dir.join("package.json")) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                    for field in ["style", "main"] {
                        if let Some(entry) = json.get(field).and_then(|v| v.as_str()) {
                            for c in css_file_candidates(&pkg_dir.join(entry)) {
                                if c.is_file() && c.extension().is_some_and(|e| e == "css") {
                                    return Some(c);
                                }
                            }
                        }
                    }
                }
            }
            let idx = pkg_dir.join("index.css");
            return idx.is_file().then_some(idx);
        }
        for c in css_file_candidates(&pkg_dir.join(rest)) {
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

fn split_package_specifier(spec: &str) -> Option<(&str, &str)> {
    let mut parts = spec.splitn(if spec.starts_with('@') { 3 } else { 2 }, '/');
    let pkg = if spec.starts_with('@') {
        let scope = parts.next()?;
        let name = parts.next()?;
        &spec[..scope.len() + 1 + name.len()]
    } else {
        parts.next()?
    };
    let rest = spec.get(pkg.len() + 1..).unwrap_or("");
    Some((pkg, rest))
}

/// Rewrite the relative `url()` / `@import` paths of a stylesheet that lives in
/// `from_dir` so they are correct from `to_dir` (where it is being inlined).
fn rebase_to_dir(
    css: &str,
    file: &Path,
    from_dir: &Path,
    to_dir: &Path,
    resolve: &CssResolve<'_>,
) -> Result<String, String> {
    if !(css.contains("url(") || css.contains("@import")) || from_dir == to_dir {
        return Ok(css.to_string());
    }
    let name = file.to_string_lossy().into_owned();
    let warnings = Arc::new(RwLock::new(Vec::new()));
    let stylesheet = StyleSheet::parse(
        css,
        ParserOptions {
            filename: name.clone(),
            error_recovery: true,
            warnings: Some(Arc::clone(&warnings)),
            ..ParserOptions::default()
        },
    )
    .map_err(|err| format!("css parse error in {name}: {err}"))?;
    report_css_warnings(&name, &warnings);
    let result = stylesheet
        .to_css(PrinterOptions {
            analyze_dependencies: Some(DependencyOptions::default()),
            ..PrinterOptions::default()
        })
        .map_err(|err| format!("css print error in {name}: {err}"))?;
    let out = result.code;
    let deps = result.dependencies.unwrap_or_default();
    let mut pairs = Vec::with_capacity(deps.len());
    for dep in deps {
        let (placeholder, orig) = match dep {
            Dependency::Url(u) => (u.placeholder, u.url),
            Dependency::Import(i) => (i.placeholder, i.url),
        };
        // Aliased specs are not relative to the file: they keep their spelling
        // for the entry's own resolution step.
        let replacement =
            if rebase_relative(&orig, "/x").is_none() || resolve.alias_spec(&orig).is_some() {
                orig
            } else {
                let (path, suffix) = match orig.find(['?', '#']) {
                    Some(i) => (&orig[..i], &orig[i..]),
                    None => (orig.as_str(), ""),
                };
                format!("{}{}", relative_path(to_dir, &from_dir.join(path)), suffix)
            };
        pairs.push((placeholder, replacement));
    }
    Ok(substitute_placeholders(out, &pairs))
}

/// `target` expressed relative to `from_dir`, with `/` separators and a leading
/// `./` when it does not start with `..`.
fn relative_path(from_dir: &Path, target: &Path) -> String {
    let norm = |p: &Path| -> Vec<String> {
        let mut segs: Vec<String> = Vec::new();
        for c in p.components() {
            match c {
                std::path::Component::ParentDir => {
                    segs.pop();
                }
                std::path::Component::CurDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_) => {}
                std::path::Component::Normal(s) => segs.push(s.to_string_lossy().into_owned()),
            }
        }
        segs
    };
    let from = norm(from_dir);
    let to = norm(target);
    let common = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<String> =
        std::iter::repeat_n("..".to_string(), from.len() - common).collect();
    parts.extend(to[common..].iter().cloned());
    let joined = parts.join("/");
    if joined.starts_with("..") {
        joined
    } else {
        format!("./{joined}")
    }
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn css_base_dir(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    if !path.starts_with('/') {
        return None;
    }
    match path.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(i) => Some(path[..i].to_string()),
        None => None,
    }
}

/// The server-absolute url a `url()` / `@import` spec of a stylesheet served
/// from `base_dir` stands for: an aliased path becomes the url of that file,
/// a relative path is joined to `base_dir`, anything else (root-absolute,
/// external, data:) is kept.
fn dev_url_of(spec: &str, base_dir: &str, resolve: &CssResolve<'_>) -> String {
    let (path, suffix) = match spec.find(['?', '#']) {
        Some(i) => (&spec[..i], &spec[i..]),
        None => (spec, ""),
    };
    if let Some(file) = resolve.alias_path(path) {
        return format!("{}{suffix}", resolve.dev_url(&file));
    }
    rebase_relative(spec, base_dir).unwrap_or_else(|| spec.to_string())
}

/// Replace every dependency placeholder lightningcss printed into `code` with its
/// rewritten url, in one left-to-right pass. In dependency mode the printer always
/// emits a placeholder as a quoted string (`url("PH")`, `@import "PH"`, and bare
/// `"PH" 1x` inside `image-set()`, values/image.rs), so the pass looks at the text
/// between each `"` and the next one; the anchor must stay the bare quote for that
/// reason. Placeholders may repeat (one `Dependency` per occurrence) or not occur;
/// the first replacement recorded for a placeholder wins, as the sequential
/// `str::replace` loop this supersedes did. A token that is not a placeholder
/// (a user string, an escaped quote) simply misses the map.
///
/// Where this knowingly differs from that loop, the old output was already
/// corrupted CSS (a 32-bit placeholder hash colliding with user text): an
/// unquoted match is not replaced, an inserted replacement is not re-scanned,
/// and an unterminated `"PH` at the end of the input is left as written.
fn substitute_placeholders(code: String, pairs: &[(String, String)]) -> String {
    if pairs.is_empty() {
        return code;
    }
    let mut map: HashMap<&str, &str> = HashMap::with_capacity(pairs.len());
    for (p, r) in pairs {
        if !p.is_empty() {
            map.entry(p.as_str()).or_insert(r.as_str());
        }
    }
    let extra: usize = pairs
        .iter()
        .map(|(p, r)| r.len().saturating_sub(p.len()))
        .sum();
    let mut out = String::with_capacity(code.len() + extra);
    let mut copied = 0usize; // start of the text not yet appended to `out`
    let mut scan = 0usize; // where the next `"` search begins
    while let Some(q) = code[scan..].find('"') {
        let start = scan + q + 1; // first byte after the opening quote
        let Some(n) = code[start..].find('"') else {
            break;
        };
        let end = start + n; // the closing quote
        if let Some(r) = map.get(&code[start..end]) {
            out.push_str(&code[copied..start]);
            out.push_str(r);
            copied = end; // the closing quote stays in the copied tail
            scan = end + 1;
        } else {
            scan = start; // the closing quote may open the next token
        }
    }
    out.push_str(&code[copied..]);
    out
}

fn rebase_relative(spec: &str, base_dir: &str) -> Option<String> {
    if spec.is_empty()
        || spec.starts_with('/')
        || spec.starts_with('#')
        || spec.starts_with("data:")
        || spec.starts_with("//")
        || spec.contains("://")
    {
        return None;
    }
    let (path, suffix) = match spec.find(['?', '#']) {
        Some(i) => (&spec[..i], &spec[i..]),
        None => (spec, ""),
    };
    Some(format!("{}{}", posix_join(base_dir, path), suffix))
}

fn posix_join(base_dir: &str, rel: &str) -> String {
    let mut segments: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    format!("/{}", segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_css_passes_through_and_minifies() {
        let out = compile_css("/styles.css", "body {\n  color: red;\n}\n", true).unwrap();
        assert_eq!(out.css, "body{color:red}");
        assert!(out.exports.is_none());
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
    fn is_css_module_matches_only_the_filename() {
        assert!(is_css_module("/src/app.module.css"));
        assert!(is_css_module("app.module.scss"));
        assert!(is_css_module("/a/b.module.css?used"));
        assert!(!is_css_module("/src/styles.css"));
        assert!(!is_css_module("/module.styles/app.css"));
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
    fn css_modules_scope_and_export_class_names() {
        let out = compile_css(
            "/src/Counter.module.css",
            ".button { padding: 1rem; } .button:hover { opacity: 0.9; }",
            false,
        )
        .unwrap();
        let exports = out.exports.expect("module exports");
        assert_eq!(exports.len(), 1);
        let (name, scoped) = &exports[0];
        assert_eq!(name, "button");
        assert_ne!(scoped, "button", "class must be scoped: {scoped}");
        assert!(out.css.contains(scoped.as_str()), "{}", out.css);
    }

    fn with_modules(modules: CssModulesOptions) -> CssResolveConfig {
        CssResolveConfig {
            modules,
            minify: true,
            ..Default::default()
        }
    }

    #[test]
    fn css_modules_locals_convention_shapes_the_export_map() {
        let src = ".my-class { color: red } .foo_bar { color: blue } .Plain { color: green }";
        let keys = |conv: &str| {
            let cfg = with_modules(CssModulesOptions {
                locals_convention: Some(conv.into()),
                ..Default::default()
            });
            compile_css_with("/src/a.module.css", src, &cfg.as_ref())
                .unwrap()
                .exports
                .unwrap()
                .into_iter()
                .map(|(k, _)| k)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            keys("camelCase"),
            ["Plain", "fooBar", "foo_bar", "my-class", "myClass", "plain"]
        );
        assert_eq!(keys("camelCaseOnly"), ["fooBar", "myClass", "plain"]);
        assert_eq!(keys("dashes"), ["Plain", "foo_bar", "my-class", "myClass"]);
        assert_eq!(keys("dashesOnly"), ["Plain", "foo_bar", "myClass"]);
        // Converted keys carry the same scoped value as the original.
        let cfg = with_modules(CssModulesOptions {
            locals_convention: Some("camelCase".into()),
            ..Default::default()
        });
        let out = compile_css_with("/src/a.module.css", src, &cfg.as_ref())
            .unwrap()
            .exports
            .unwrap();
        let get = |k: &str| {
            out.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(get("my-class"), get("myClass"));
    }

    #[test]
    fn css_modules_generate_scoped_name_pattern_is_honored() {
        let cfg = with_modules(CssModulesOptions {
            generate_scoped_name: Some("[local]__[hash:base64:5]".into()),
            ..Default::default()
        });
        let out = compile_css_with(
            "/src/Btn.module.css",
            ".button { color: red }",
            &cfg.as_ref(),
        )
        .unwrap();
        let (_, scoped) = &out.exports.unwrap()[0];
        assert!(scoped.starts_with("button__"), "{scoped}");
        assert!(!scoped.contains("Btn"), "{scoped}");
        let cfg = with_modules(CssModulesOptions {
            generate_scoped_name: Some("app-[name]-[local]".into()),
            ..Default::default()
        });
        let out = compile_css_with(
            "/src/Btn.module.css",
            ".button { color: red }",
            &cfg.as_ref(),
        )
        .unwrap();
        assert_eq!(out.exports.unwrap()[0].1, "app-Btn-module-button");
        // An unsupported pattern falls back to the default instead of failing.
        let cfg = with_modules(CssModulesOptions {
            generate_scoped_name: Some("[nope]".into()),
            ..Default::default()
        });
        let out = compile_css_with(
            "/src/Btn.module.css",
            ".button { color: red }",
            &cfg.as_ref(),
        )
        .unwrap();
        assert!(out.exports.unwrap()[0].1.starts_with("Btn-module_button_"));
    }

    #[test]
    fn css_modules_global_scope_and_global_module_paths_compile_unscoped() {
        let cfg = with_modules(CssModulesOptions {
            global_scope: true,
            ..Default::default()
        });
        let out =
            compile_css_with("/src/a.module.css", ".x { color: red }", &cfg.as_ref()).unwrap();
        assert_eq!(out.css, ".x{color:red}");
        assert_eq!(
            out.exports,
            Some(Vec::new()),
            "still a module to its importer, with no locals"
        );

        let cfg = with_modules(CssModulesOptions {
            global_module_paths: vec![r"global\.module\.css$".into()],
            ..Default::default()
        });
        let g = compile_css_with(
            "/src/theme.global.module.css",
            ".x { color: red }",
            &cfg.as_ref(),
        )
        .unwrap();
        assert_eq!(g.css, ".x{color:red}");
        assert_eq!(g.exports, Some(Vec::new()));
        let scoped =
            compile_css_with("/src/a.module.css", ".x { color: red }", &cfg.as_ref()).unwrap();
        assert_ne!(
            scoped.css, ".x{color:red}",
            "non-matching modules stay scoped"
        );
    }

    #[test]
    fn css_modules_composes_locals_globals_and_other_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/base.module.css"),
            ".base { padding: 1px } .more { composes: base; margin: 0 }",
        )
        .unwrap();
        let cfg = CssResolveConfig {
            root: dir.path().to_path_buf(),
            ..Default::default()
        };
        std::fs::write(
            dir.path().join("src/a.module.css"),
            ".a { color: red } .b { composes: a; color: blue } .c { composes: g from global; } .d { composes: more from \"./base.module.css\"; }",
        )
        .unwrap();
        let out = compile_css_with(
            "/src/a.module.css",
            &std::fs::read_to_string(dir.path().join("src/a.module.css")).unwrap(),
            &cfg.as_ref(),
        )
        .unwrap();
        let exports = out.exports.unwrap();
        let get = |k: &str| {
            exports
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(
            get("b"),
            format!("{} {}", get("b").split(' ').next().unwrap(), get("a"))
        );
        assert_eq!(get("c").split(' ').nth(1), Some("g"));
        let base = compile_css_with(
            "/src/base.module.css",
            &std::fs::read_to_string(dir.path().join("src/base.module.css")).unwrap(),
            &cfg.as_ref(),
        )
        .unwrap()
        .exports
        .unwrap();
        let base_more = base
            .iter()
            .find(|(n, _)| n == "more")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert!(
            base_more.contains(' '),
            "more composes base transitively: {base_more}"
        );
        let d = get("d");
        assert!(
            d.ends_with(&base_more),
            "d = {d}, expected suffix {base_more}"
        );
    }

    #[test]
    fn css_modules_scoped_name_matches_ssr_loader() {
        // The retired node SSR loader (its JS cssModuleExports mirror)
        // recomputes these names in JS so server rendering agrees with the
        // class map the client is served; ssr-loader-css-modules.test.mjs pins
        // the same literals. If this assertion changes (lightningcss upgrade,
        // pattern change), the loader must be updated to match.
        let out = compile_css("/src/Counter.module.css", ".button { color: red; }", false).unwrap();
        let exports = out.exports.expect("module exports");
        assert_eq!(
            exports,
            vec![(
                "button".to_string(),
                "Counter-module_button_EjW_Uq".to_string()
            )]
        );
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
    fn inlines_relative_and_package_imports_and_rebases_urls() {
        let base = std::env::temp_dir().join(format!("oj-css-imp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("src/base")).unwrap();
        std::fs::create_dir_all(base.join("node_modules/normalize-fake")).unwrap();
        std::fs::write(base.join("src/vars.css"), ":root { --x: 1; }\n").unwrap();
        std::fs::write(
            base.join("src/base/reset.css"),
            "@import \"../vars.css\";\n.reset { background: url(./dot.png); }\n",
        )
        .unwrap();
        std::fs::write(
            base.join("node_modules/normalize-fake/package.json"),
            r#"{"name":"normalize-fake","style":"n.css"}"#,
        )
        .unwrap();
        std::fs::write(
            base.join("node_modules/normalize-fake/n.css"),
            ".norm { margin: 0; }\n",
        )
        .unwrap();
        std::fs::write(base.join("src/print.css"), ".print { display: none; }\n").unwrap();
        let app = base.join("src/app.css");
        let src = "@import \"./base/reset.css\";\n@import 'normalize-fake';\n@import url(https://cdn.test/x.css);\n@import \"./print.css\" print;\n@import \"./missing.css\" screen;\n.app { color: red; }\n";
        let out = inline_imports(src, &app).unwrap();
        assert!(out.contains("--x: 1"), "nested import inlined: {out}");
        assert!(out.contains(".reset"), "relative import inlined: {out}");
        assert!(
            out.contains("url(\"./base/dot.png\")") || out.contains("url(./base/dot.png)"),
            "url rebased to the entry dir: {out}"
        );
        assert!(out.contains(".norm"), "package style entry inlined: {out}");
        assert!(
            out.contains("@import url(https://cdn.test/x.css);"),
            "external import kept: {out}"
        );
        assert!(
            out.contains("@media print {\n.print { display: none; }"),
            "media import inlined as @media: {out}"
        );
        assert!(
            out.contains("@import \"./missing.css\" screen;"),
            "unresolvable media import kept: {out}"
        );
        // Kept imports are hoisted above the inlined rules (CSS requires it).
        let first_rule = out.find('{').unwrap();
        assert!(
            out.rfind("@import").unwrap() < first_rule,
            "imports hoisted first: {out}"
        );
        assert!(
            !out.contains("@import \"./base/reset.css\""),
            "inlined import removed: {out}"
        );
        assert!(
            out.contains(".app { color: red; }"),
            "own rules kept verbatim: {out}"
        );
        // Compiles and, in dev, rebases against the served url of the entry.
        let compiled = compile_css_rebased("/src/app.css", &out, true).unwrap();
        assert!(
            compiled.css.contains("/src/base/dot.png"),
            "{}",
            compiled.css
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn import_inlining_survives_cycles_and_missing_files() {
        let base = std::env::temp_dir().join(format!("oj-css-cyc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("a.css"), "@import \"./b.css\";\n.a{}").unwrap();
        std::fs::write(base.join("b.css"), "@import \"./a.css\";\n.b{}").unwrap();
        let out = inline_imports("@import \"./b.css\";\n.a{}", &base.join("a.css")).unwrap();
        assert!(out.contains(".b{}") && out.contains(".a{}"), "{out}");
        assert!(
            out.contains("@import \"./a.css\";"),
            "the cycle edge stays as written: {out}"
        );
        let missing = inline_imports("@import \"./nope.css\";\n.a{}", &base.join("a.css")).unwrap();
        assert!(
            missing.contains("@import \"./nope.css\";"),
            "unresolvable import left alone: {missing}"
        );
        assert_eq!(
            relative_path(Path::new("/p/src"), Path::new("/p/src/base/dot.png")),
            "./base/dot.png"
        );
        assert_eq!(
            relative_path(Path::new("/p/src/base"), Path::new("/p/vars.css")),
            "../../vars.css"
        );
        assert_eq!(
            split_package_specifier("@acme/tokens/src/x.css"),
            Some(("@acme/tokens", "src/x.css"))
        );
        assert_eq!(
            split_package_specifier("normalize.css"),
            Some(("normalize.css", ""))
        );
        let _ = std::fs::remove_dir_all(&base);
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

    #[test]
    fn scoped_class_names_follow_the_name_local_hash_pattern() {
        // The scoped name shows up in devtools, in snapshots and in the exports
        // map, so its shape is part of the contract.
        let out = compile_css("/src/Counter.module.css", ".button { color: red }", false).unwrap();
        let (local, scoped) = out.exports.expect("exports").into_iter().next().unwrap();
        assert_eq!(local, "button");
        // `[name]` is the file stem with its dots flattened.
        let prefix = "Counter-module_button_";
        assert!(
            scoped.starts_with(prefix),
            "expected [name]_[local]_[hash], got {scoped}"
        );
        let hash = &scoped[prefix.len()..];
        assert!(!hash.is_empty(), "no hash in {scoped}");
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "hash is not a plain token: {scoped}"
        );
        // The hash is derived from the path, not the contents: editing a
        // stylesheet must not rename its classes, or every edit would invalidate
        // the markup that already references them.
        let after_edit = compile_css("/src/Counter.module.css", ".button { color: blue }", false)
            .unwrap()
            .exports
            .expect("exports")
            .remove(0)
            .1;
        assert_eq!(scoped, after_edit, "an edit must not rename a class");
    }

    #[test]
    fn alias_matches_whole_specifier_or_slash_prefix_like_rollup_alias() {
        let alias = vec![
            ("@".to_string(), "./src".to_string()),
            ("@components".to_string(), "/abs/components".to_string()),
            ("react".to_string(), "preact/compat".to_string()),
        ];
        let r = CssResolve {
            root: Some(Path::new("/proj")),
            public_dir: None,
            alias: &alias,
            ..CssResolve::default()
        };
        assert_eq!(
            r.alias_spec("@/img.png").as_deref(),
            Some("/proj/src/img.png")
        );
        assert_eq!(r.alias_spec("@").as_deref(), Some("/proj/src"));
        // `@components/x` must not be eaten by the shorter `@` alias.
        assert_eq!(
            r.alias_spec("@components/btn.css").as_deref(),
            Some("/abs/components/btn.css")
        );
        assert_eq!(
            r.alias_spec("@scope/pkg/x.css"),
            None,
            "not a `find/` prefix match"
        );
        // A package alias stays a bare specifier (no path).
        assert_eq!(
            r.alias_spec("react/x.css").as_deref(),
            Some("preact/compat/x.css")
        );
        assert!(r.alias_path("react/x.css").is_none());
        assert_eq!(
            r.alias_path("@/a.css"),
            Some(PathBuf::from("/proj/src/a.css"))
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

    /// The sequential `str::replace` loop `substitute_placeholders` replaced.
    fn sequential_replace(code: &str, pairs: &[(String, String)]) -> String {
        pairs
            .iter()
            .fold(code.to_string(), |c, (p, r)| c.replace(p, r))
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(p, r)| (p.to_string(), r.to_string()))
            .collect()
    }

    /// Every `"` followed by exactly six placeholder-alphabet bytes and a closing
    /// `"`: the shape of an unsubstituted lightningcss dependency placeholder
    /// (and of a six-character user string, which callers exclude by value).
    fn quoted_six_char_tokens(css: &str) -> Vec<&str> {
        let b = css.as_bytes();
        (0..b.len())
            .filter(|&i| {
                b[i] == b'"'
                    && i + 7 < b.len()
                    && b[i + 7] == b'"'
                    && b[i + 1..i + 7]
                        .iter()
                        .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
            })
            .map(|i| &css[i + 1..i + 7])
            .collect()
    }

    #[test]
    fn substitute_placeholders_handles_edge_shapes() {
        // Empty pairs: the input comes back as-is, without a copy.
        let input = ".a{content:\"abcdef\"}".to_string();
        let (ptr, cap) = (input.as_ptr(), input.capacity());
        let same = substitute_placeholders(input, &[]);
        assert_eq!((same.as_ptr(), same.capacity()), (ptr, cap));
        assert_eq!(same, ".a{content:\"abcdef\"}");

        let cases = vec![
            ("", pairs(&[("abcdef", "/a.png")]), ""),
            (
                ".a{color:red}",
                pairs(&[("abcdef", "/a.png")]),
                ".a{color:red}",
            ),
            // Placeholder at byte 0 and as the final quoted token.
            (
                "\"abcdef\" .a{x:1}\"abcdef\"",
                pairs(&[("abcdef", "/a.png")]),
                "\"/a.png\" .a{x:1}\"/a.png\"",
            ),
            // Duplicate placeholder with two replacements: first wins, both occurrences replaced.
            (
                ".a{background:url(\"abcdef\")}.b{background:url(\"abcdef\")}",
                pairs(&[("abcdef", "/first.png"), ("abcdef", "/second.png")]),
                ".a{background:url(\"/first.png\")}.b{background:url(\"/first.png\")}",
            ),
            // Zero-occurrence placeholder and a same-length user string.
            (
                ".a{content:\"qqqqqq\";background:url(\"abcdef\")}",
                pairs(&[("zzzzzz", "/z.png"), ("abcdef", "/a.png")]),
                ".a{content:\"qqqqqq\";background:url(\"/a.png\")}",
            ),
            // Escaped quote inside a user string before a placeholder.
            (
                ".a{content:\"a\\\"b\";background:url(\"Ab-_09\")}",
                pairs(&[("Ab-_09", "/x/y.svg")]),
                ".a{content:\"a\\\"b\";background:url(\"/x/y.svg\")}",
            ),
            // Adjacent quoted tokens.
            (
                "\"P1P1P1\"\"P2P2P2\"",
                pairs(&[("P1P1P1", "/1"), ("P2P2P2", "/2")]),
                "\"/1\"\"/2\"",
            ),
            // Multibyte neighbours and a multibyte replacement.
            (
                ".a::before{content:\"héllo\";background:url(\"Ab-_09\")}",
                pairs(&[("Ab-_09", "/ünï/y.svg")]),
                ".a::before{content:\"héllo\";background:url(\"/ünï/y.svg\")}",
            ),
            // Empty replacement.
            (
                ".a{x:url(\"abcdef\")}",
                pairs(&[("abcdef", "")]),
                ".a{x:url(\"\")}",
            ),
        ];
        for (code, pairs, want) in cases {
            let got = substitute_placeholders(code.to_string(), &pairs);
            assert_eq!(got, want, "input {code:?}");
            assert_eq!(
                got,
                sequential_replace(code, &pairs),
                "diverges from the sequential loop on {code:?}"
            );
        }

        // An unterminated `"PH` at the end of the input is left as written.
        let code = ".a{background:url(\"abcdef\")}\"abcdef";
        let got = substitute_placeholders(code.to_string(), &pairs(&[("abcdef", "/a.png")]));
        assert_eq!(got, ".a{background:url(\"/a.png\")}\"abcdef");
    }

    #[test]
    fn substitute_placeholders_matches_sequential_replace_on_printed_css() {
        let src = "@import \"./x.css\";\n\
                   @font-face { font-family: F; src: url(\"./f.woff2\") format(\"woff2\"); }\n\
                   .a { background: url(./a.png); }\n\
                   .b { background: url(\"./b.png\"); }\n\
                   .c { background: url(./a.png); }\n\
                   .d { background: image-set(\"./a.png\" 1x, \"./b.png\" 2x); }\n\
                   .e { background: -webkit-image-set(url(\"./c.png\") 1x, url(\"./d.png\") 2x); }\n\
                   .f::before { content: \"abcdef\"; }\n\
                   .g::before { content: \"a\\\"b\"; }\n\
                   .h { background: url(\"data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg'/%3e\"); }\n";
        let stylesheet = StyleSheet::parse(
            src,
            ParserOptions {
                filename: "/src/x.css".into(),
                ..ParserOptions::default()
            },
        )
        .unwrap();
        let result = stylesheet
            .to_css(PrinterOptions {
                analyze_dependencies: Some(DependencyOptions::default()),
                ..PrinterOptions::default()
            })
            .unwrap();
        let deps = result.dependencies.unwrap();
        let pairs: Vec<(String, String)> = deps
            .into_iter()
            .map(|dep| match dep {
                Dependency::Url(u) => (u.placeholder, format!("/src/{}", u.url)),
                Dependency::Import(i) => (i.placeholder, format!("/src/{}", i.url)),
            })
            .collect();
        assert!(
            pairs.len() >= 9,
            "one Dependency per printed url/import: {pairs:?}"
        );
        assert!(pairs.iter().all(|(p, _)| p.len() == 6), "{pairs:?}");
        // One quoted placeholder per Dependency, plus the user string.
        assert_eq!(
            quoted_six_char_tokens(&result.code).len(),
            pairs.len() + 1,
            "{}",
            result.code
        );

        let got = substitute_placeholders(result.code.clone(), &pairs);
        assert_eq!(got, sequential_replace(&result.code, &pairs));
        // Only the same-width user string survives as a six-char quoted token.
        assert_eq!(quoted_six_char_tokens(&got), vec!["abcdef"], "{got}");
        assert!(
            got.contains("/src/./a.png")
                && got.contains("/src/./x.css")
                && got.contains("/src/./f.woff2"),
            "{got}"
        );
        assert!(
            got.contains("/src/data:image/svg+xml"),
            "the test maps every dependency, data: included: {got}"
        );
    }

    #[test]
    fn dev_compile_rewrites_a_url_used_twice_and_an_import() {
        // Distinct declarations keep the two rules from being merged by minify.
        let src = "@import \"./b.css\";\n\
                   .a { background: url(./a.png); color: red; }\n\
                   .b { background: url(\"./a.png\"); color: blue; }\n";
        let out = compile_css_dev("/src/x.css", src, false, &CssResolve::default())
            .unwrap()
            .css;
        assert_eq!(
            out.matches("url(\"/src/a.png\")").count(),
            2,
            "both occurrences rewritten: {out}"
        );
        assert!(out.contains("@import \"/src/b.css\""), "{out}");
        assert!(
            quoted_six_char_tokens(&out).is_empty(),
            "a placeholder survived substitution: {out}"
        );
    }

    #[test]
    fn imports_resolve_through_alias_root_and_public_dir() {
        let base = std::env::temp_dir().join(format!("oj-css-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("src/ui")).unwrap();
        std::fs::create_dir_all(base.join("public")).unwrap();
        std::fs::write(base.join("src/vars.css"), ":root { --v: 1; }\n").unwrap();
        std::fs::write(
            base.join("src/ui/theme.css"),
            ".theme { color: red; background: url(@/img.png); }\n",
        )
        .unwrap();
        std::fs::write(base.join("public/vendor.css"), ".vendor { margin: 0; }\n").unwrap();
        // A same-named file under the root must lose to the public one.
        std::fs::write(base.join("vendor.css"), ".wrong { margin: 1px; }\n").unwrap();
        let alias = vec![("@".to_string(), "./src".to_string())];
        let public = base.join("public");
        let r = CssResolve {
            root: Some(&base),
            public_dir: Some(&public),
            alias: &alias,
            ..CssResolve::default()
        };
        let dir = base.join("src/ui");
        assert_eq!(
            resolve_css_import_with("@/vars.css", &dir, &r),
            Some(base.join("src/vars.css"))
        );
        assert_eq!(
            resolve_css_import_with("@/vars", &dir, &r),
            Some(base.join("src/vars.css")),
            "extension probed"
        );
        assert_eq!(
            resolve_css_import_with("/src/vars.css", &dir, &r),
            Some(base.join("src/vars.css"))
        );
        assert_eq!(
            resolve_css_import_with("/vendor.css", &dir, &r),
            Some(base.join("public/vendor.css")),
            "public dir wins"
        );
        assert_eq!(resolve_css_import_with("/nope.css", &dir, &r), None);
        // Without a root the same specs stay unresolved (kept as written).
        assert_eq!(resolve_css_import("@/vars.css", &dir), None);
        assert_eq!(resolve_css_import("/src/vars.css", &dir), None);

        let entry = base.join("src/ui/app.css");
        let src = "@import \"@/vars.css\";\n@import \"/src/ui/theme.css\";\n@import '/vendor.css';\n.app { color: blue; }\n";
        let out = inline_imports_with(src, &entry, &r).unwrap();
        assert!(out.contains("--v: 1"), "aliased import inlined: {out}");
        assert!(
            out.contains(".theme"),
            "root-absolute import inlined: {out}"
        );
        assert!(
            out.contains(".vendor") && !out.contains(".wrong"),
            "public import inlined: {out}"
        );
        assert!(
            out.contains("url(@/img.png)") || out.contains("url(\"@/img.png\")"),
            "aliased url inside an inlined file is not rebased as relative: {out}"
        );
        assert!(!out.contains("@import"), "{out}");
        // The dev compile then turns the aliased url into the served path.
        let compiled = compile_css_dev("/src/ui/app.css", &out, false, &r)
            .unwrap()
            .css;
        assert!(compiled.contains("/src/img.png"), "{compiled}");
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

    #[test]
    fn css_modules_esm_names_legal_identifiers_and_keeps_the_whole_map_default() {
        let exports = vec![
            ("button".to_string(), "m_button_h".to_string()),
            ("my-class".to_string(), "m_my-class_h".to_string()),
            ("default".to_string(), "m_default_h".to_string()),
            ("class".to_string(), "m_class_h".to_string()),
            ("Map".to_string(), "m_Map_h".to_string()),
            ("_private".to_string(), "m__private_h".to_string()),
            ("$x".to_string(), "m_x_h".to_string()),
            ("1st".to_string(), "m_1st_h".to_string()),
        ];
        let js = css_modules_esm(&exports);
        assert!(js.contains("export const button = \"m_button_h\";"), "{js}");
        assert!(
            js.contains("export const _private = ") && js.contains("export const $x = "),
            "{js}"
        );
        for bad in ["my-class", "default", "class", "Map", "1st"] {
            assert!(
                !js.contains(&format!("export const {bad} ")),
                "{bad} must not be a named export: {js}"
            );
        }
        assert!(js.contains("export default {"), "{js}");
        for key in [
            "button", "my-class", "default", "class", "Map", "_private", "$x", "1st",
        ] {
            assert!(
                js.contains(&format!("\"{key}\":\"m_")),
                "{key} in the default map: {js}"
            );
        }
        assert_eq!(css_modules_esm(&[]), "export default {};\n");
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
