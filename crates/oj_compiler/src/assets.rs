// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The one list of file types oj treats as static assets (a module import
//! yields the file's URL), shared by the dev server and the production build so
//! the two never diverge. Mirrors Vite's `KNOWN_ASSET_TYPES`; matching is
//! case-insensitive like Vite's `DEFAULT_ASSETS_RE` (`/i`), so a camera's
//! `photo.JPG` is an asset too.

pub const KNOWN_ASSET_TYPES: &[&str] = &[
    // images
    "apng",
    "bmp",
    "png",
    "jpg",
    "jpeg",
    "jfif",
    "pjpeg",
    "pjp",
    "gif",
    "svg",
    "ico",
    "webp",
    "avif",
    "cur",
    "jxl",
    // media
    "mp4",
    "webm",
    "ogg",
    "mp3",
    "wav",
    "flac",
    "aac",
    "opus",
    "mov",
    "m4a",
    "vtt",
    // fonts
    "woff",
    "woff2",
    "eot",
    "ttf",
    "otf",
    // other
    "webmanifest",
    "pdf",
    "txt",
];

/// Whether a file extension (no dot) names a known asset type, ignoring case.
pub fn is_asset_ext(ext: &str) -> bool {
    KNOWN_ASSET_TYPES
        .iter()
        .any(|k| k.eq_ignore_ascii_case(ext))
}

/// The extension of a url or path with any `?query` / `#hash` removed, e.g.
/// `"/src/a.PNG?url"` -> `Some("PNG")`.
pub fn clean_ext(url: &str) -> Option<&str> {
    let clean = url.split(['?', '#']).next().unwrap_or(url);
    let name = clean.rsplit(['/', '\\']).next().unwrap_or(clean);
    let (_, ext) = name.rsplit_once('.')?;
    (!ext.is_empty()).then_some(ext)
}

/// Whether a url or path (query and hash ignored) names a known asset type.
pub fn is_asset_url(url: &str) -> bool {
    clean_ext(url).is_some_and(is_asset_ext)
}

/// Vite's `assetsInclude`, resolved: the files beyond the known types whose
/// import is their URL (`config.assetsInclude(file)` in Vite: the
/// `DEFAULT_ASSETS_RE` test or `createFilter(config.assetsInclude)`).
/// Patterns are read as that `createFilter` (@rollup/pluginutils, called
/// with no `resolve` option) reads them:
///
/// - a glob that starts with `**` or is absolute is used as written, so
///   `**/*.gltf` names a `.gltf` anywhere, in the app or outside it;
/// - any other glob is resolved against the directory the process was
///   started from (`resolve("")`), not the app root;
/// - a RegExp is tested against the whole path;
/// - picomatch runs with `dot: true`, and an id holding a NUL names nothing.
///
/// Installed from the resolved config ([`install_assets_include`]), one
/// filter per app root, as each Vite server consults its own resolved
/// config: a lookup names the app it is for, so a process that loads
/// several apps (the dev server's tests boot many servers side by side)
/// never judges one app's file by another's patterns.
struct AssetsInclude {
    /// The root as it was given and as it resolves, so either spelling of
    /// one app finds its filter.
    roots: Vec<std::path::PathBuf>,
    globs: globset::GlobSet,
    regexes: Vec<regex::Regex>,
}

impl AssetsInclude {
    fn matches(&self, path: &str) -> bool {
        self.globs.is_match(path) || self.regexes.iter().any(|re| re.is_match(path))
    }

    /// By the bytes of the path first: this runs for every import a server
    /// rewrites, and a root is nearly always handed over as it was stored.
    fn is_for(&self, root: &std::path::Path) -> bool {
        self.roots
            .iter()
            .any(|r| r.as_os_str() == root.as_os_str() || r == root)
    }

    /// `path` (absolute or a served url's, `/`-separated) under any spelling
    /// of the root: as it is, moved from the spelling it is under to the
    /// others (a glob resolved against the start directory names the root
    /// as the system spells it, a path may name it through a symlink), and,
    /// for a path under none that is no file elsewhere, joined under each.
    fn matches_under_the_root(&self, path: &str) -> bool {
        if self.matches(path) {
            return true;
        }
        let as_path = std::path::Path::new(path);
        let slashed = |p: std::path::PathBuf| p.to_string_lossy().replace('\\', "/");
        let mut under_a_root = false;
        for root in &self.roots {
            let Ok(rest) = as_path.strip_prefix(root) else {
                continue;
            };
            under_a_root = true;
            for other in self.roots.iter().filter(|other| *other != root) {
                if self.matches(&slashed(other.join(rest))) {
                    return true;
                }
            }
        }
        if under_a_root || as_path.exists() {
            return false;
        }
        let rest = path.trim_start_matches('/');
        self.roots
            .iter()
            .any(|root| self.matches(&slashed(root.join(rest))))
    }
}

/// A relative glob against its base, as `posix.join(base, glob)` in
/// getMatcherString: `.` segments dropped and each `..` taking the segment
/// before it, so `../shared/*.obj` names the base's sibling. The base's own
/// characters are literal (pluginutils escapes them for picomatch).
fn resolved_glob(base: &str, glob: &str) -> String {
    let mut segments: Vec<String> = base
        .split('/')
        .filter(|s| !s.is_empty())
        .map(globset::escape)
        .collect();
    for segment in glob.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other.to_string()),
        }
    }
    let joined = segments.join("/");
    if base.starts_with('/') {
        format!("/{joined}")
    } else {
        joined
    }
}

/// Each root's filter, the last installed for it.
static ASSETS_INCLUDE: std::sync::RwLock<Vec<AssetsInclude>> = std::sync::RwLock::new(Vec::new());

/// Installs `root`'s `assetsInclude` for [`is_included_asset`]: string
/// globs and `{ "regex": source, "flags": flags }` objects, as the config
/// extraction carries them. `base` is the directory relative globs resolve
/// against: the one the process was started from, as Vite's. An empty list
/// names nothing for `root`; every other root's filter stays as it is. A
/// pattern that does not parse is skipped, as picomatch would never have
/// matched it.
pub fn install_assets_include(
    patterns: &[serde_json::Value],
    root: &std::path::Path,
    base: &std::path::Path,
) {
    let mut globs = globset::GlobSetBuilder::new();
    let mut regexes = Vec::new();
    let base_slash = base.to_string_lossy().replace('\\', "/");
    for p in patterns {
        match p {
            serde_json::Value::String(glob) => {
                // getMatcherString: `**...` and absolute patterns stand as
                // written, the rest join the resolution base.
                let pattern = if glob.starts_with("**") || std::path::Path::new(glob).is_absolute()
                {
                    glob.replace('\\', "/")
                } else {
                    resolved_glob(&base_slash, &glob.replace('\\', "/"))
                };
                // picomatch: `*` stops at `/`, `**` crosses directories,
                // and createFilter matches dotfiles (`dot: true`).
                if let Ok(g) = globset::GlobBuilder::new(&pattern)
                    .literal_separator(true)
                    .build()
                {
                    globs.add(g);
                }
            }
            serde_json::Value::Object(o) => {
                let Some(source) = o.get("regex").and_then(|v| v.as_str()) else {
                    continue;
                };
                let flags = o.get("flags").and_then(|v| v.as_str()).unwrap_or("");
                if let Ok(re) = regex::RegexBuilder::new(source)
                    .case_insensitive(flags.contains('i'))
                    .multi_line(flags.contains('m'))
                    .dot_matches_new_line(flags.contains('s'))
                    .build()
                {
                    regexes.push(re);
                }
            }
            _ => {}
        }
    }
    let mut roots = vec![root.to_path_buf()];
    if let Ok(real) = std::fs::canonicalize(root) {
        if real != root {
            roots.push(real);
        }
    }
    let installed = AssetsInclude {
        roots,
        globs: globs.build().unwrap_or_else(|_| globset::GlobSet::empty()),
        regexes,
    };
    let mut filters = ASSETS_INCLUDE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Either spelling of the root replaces the filter it had.
    filters.retain(|f| !installed.roots.iter().any(|r| f.is_for(r)));
    filters.push(installed);
}

/// Whether the `assetsInclude` of the app at `root` names this file (query
/// and hash ignored): Vite's `config.assetsInclude(cleanUrl(id))` beyond
/// the known types. The path is judged as it is, as Vite judges the id it
/// is handed; a served url's root-relative path (`/src/x.gltf`, no file at
/// the filesystem root) is tried under the root too, where a glob resolved
/// against a directory finds it. An app that installed no filter (no
/// config, or one without `assetsInclude`) names nothing.
pub fn is_included_asset(root: &std::path::Path, path: &str) -> bool {
    let filters = ASSETS_INCLUDE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(filter) = filters.iter().find(|f| f.is_for(root)) else {
        return false;
    };
    if filter.globs.is_empty() && filter.regexes.is_empty() {
        return false;
    }
    let clean = path
        .split(['?', '#'])
        .next()
        .unwrap_or(path)
        .replace('\\', "/");
    if clean.is_empty() || clean.contains('\0') {
        return false;
    }
    filter.matches_under_the_root(&clean)
}

/// Whether a path or url is an asset of the app at `root`: a known type by
/// its extension, or a file its config's `assetsInclude` names.
pub fn is_asset_path(root: &std::path::Path, path: &str) -> bool {
    is_asset_url(path) || is_included_asset(root, path)
}

/// MIME type for an asset extension (case-insensitive), used for `data:` URLs
/// and for serving. Unknown types are `application/octet-stream`.
pub fn asset_mime(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "apng" => "image/apng",
        "jpg" | "jpeg" | "jfif" | "pjpeg" | "pjp" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "jxl" => "image/jxl",
        "ico" | "cur" => "image/x-icon",
        "bmp" => "image/bmp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "mp4" | "m4a" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "vtt" => "text/vtt",
        "webmanifest" => "application/manifest+json",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "css" => "text/css",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_vite_known_types_case_insensitively() {
        for ext in [
            "png",
            "PNG",
            "Jpg",
            "webmanifest",
            "pdf",
            "flac",
            "m4a",
            "aac",
            "opus",
            "vtt",
            "cur",
            "jxl",
            "txt",
        ] {
            assert!(is_asset_ext(ext), "{ext}");
        }
        for ext in ["ts", "tsx", "js", "css", "scss", "json", "html", "wasm", ""] {
            assert!(!is_asset_ext(ext), "{ext}");
        }
    }

    #[test]
    fn url_check_ignores_query_and_hash() {
        assert!(is_asset_url("/src/a.PNG?url"));
        assert!(is_asset_url("/img/x.svg#frag"));
        assert!(is_asset_url("C:\\\\app\\\\x.Woff2"));
        assert!(!is_asset_url("/src/a.png.tsx"));
        assert!(!is_asset_url("/src/noext"));
        assert!(!is_asset_url(""));
        assert_eq!(clean_ext("/a/b.min.JS?x"), Some("JS"));
    }

    // Vite's `assetsInclude` beyond the known types, pattern by pattern as
    // its `createFilter(config.assetsInclude)` answers (checked against
    // Vite 8's own createFilter): a `**` glob names a file anywhere, a
    // relative glob resolves against the directory the process started in,
    // a RegExp is tested against the whole path with its flags.
    #[test]
    fn configured_assets_include_names_more_files() {
        let dir = std::env::temp_dir().join(format!("oj-assets-include-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("app");
        let outside = dir.join("outside");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("x.gltf"), "x").unwrap();
        std::fs::write(outside.join("x.obj"), "x").unwrap();
        let under = |rel: &str| root.join(rel).to_string_lossy().replace('\\', "/");
        let included = |path: &str| is_included_asset(&root, path);
        // The process started in the app's root: Vite's usual run.
        install_assets_include(
            &[
                serde_json::json!("**/*.gltf"),
                serde_json::json!("./models/*.obj"),
                serde_json::json!("meshes/*.ply"),
                serde_json::json!({ "regex": "\\.BIN$", "flags": "i" }),
                serde_json::json!(7),
            ],
            &root,
            &root,
        );
        assert!(included(&under("src/scene.gltf")));
        assert!(included(&format!("{}?url", under("src/scene.gltf"))));
        assert!(included("/src/deep/scene.gltf"), "a served url's path");
        assert!(included(&under(".hidden/scene.gltf")), "dot: true");
        // createFilter("**/*.gltf")("/outside/x.gltf") is true: a `**`
        // pattern is not resolved against anything.
        assert!(
            included(&outside.join("x.gltf").to_string_lossy()),
            "a `**` glob names a file outside the root too, as in Vite"
        );
        assert!(included(&under("models/x.obj")), "a `./` pattern");
        assert!(included(&under("meshes/x.ply")), "a relative pattern");
        assert!(included("/models/x.obj"), "a served url under the root");
        assert!(!included(&under("src/models/x.obj")), "`*` stops at `/`");
        assert!(
            !included(&outside.join("x.obj").to_string_lossy()),
            "a resolved pattern names files under its base only"
        );
        assert!(included(&under("a/b.bin")), "the RegExp's `i` flag");
        assert!(
            included(&outside.join("c.BIN").to_string_lossy()),
            "a RegExp is tested against the whole path, anywhere"
        );
        assert!(!included(&under("src/scene.png")));
        assert!(!included("\0virtual:scene.gltf"));
        assert!(!included(&format!("{}\0x.gltf", under("src/"))), "a NUL");
        assert!(is_asset_path(&root, &under("src/scene.gltf")));
        assert!(is_asset_path(&root, &under("src/photo.PNG")));

        // Started elsewhere (`oj dev ./app` from the parent): a relative
        // glob resolves against that directory, a `**` one is unchanged.
        install_assets_include(
            &[
                serde_json::json!("**/*.gltf"),
                serde_json::json!("models/*.obj"),
            ],
            &root,
            &dir,
        );
        assert!(included(&under("src/scene.gltf")));
        assert!(
            !included(&under("models/x.obj")),
            "Vite resolves a relative pattern against the working directory"
        );
        assert!(included(&dir.join("models/x.obj").to_string_lossy()));

        // posix.join normalizes the pattern: `..` takes the segment before
        // it (createFilter("../shared/*.obj") names the base's sibling), `.`
        // and a doubled `/` are dropped.
        install_assets_include(
            &[
                serde_json::json!("../outside/*.obj"),
                serde_json::json!("./src/../models//*.ply"),
            ],
            &root,
            &root,
        );
        assert!(
            included(&outside.join("x.obj").to_string_lossy()),
            "`../outside/*.obj` names the root's sibling directory"
        );
        assert!(!included(&under("outside/x.obj")));
        assert!(included(&under("models/x.ply")));
        assert!(!included(&under("src/models/x.ply")));

        install_assets_include(&[], &root, &root);
        assert!(!included(&under("src/scene.gltf")), "uninstalled");
        assert!(is_asset_path(&root, &under("src/photo.PNG")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // One filter per root, and a lookup names its app: a process that loads
    // several apps' configs (the dev server's tests boot many servers in one
    // process) judges each app's files by its own config alone, as each Vite
    // server consults its own resolved config. A `**` glob and a RegExp match
    // anywhere, so nothing short of naming the app keeps one app's patterns
    // off another's files; an app that installed nothing names nothing.
    #[test]
    fn each_root_keeps_its_own_assets_include() {
        let dir =
            std::env::temp_dir().join(format!("oj-assets-include-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (first, second, third) = (dir.join("first"), dir.join("second"), dir.join("third"));
        for root in [&first, &second, &third] {
            std::fs::create_dir_all(root.join("src")).unwrap();
        }
        let at =
            |root: &std::path::Path, rel: &str| root.join(rel).to_string_lossy().replace('\\', "/");
        install_assets_include(
            &[
                serde_json::json!("**/*.splat"),
                serde_json::json!({ "regex": "\\.pointcloud$", "flags": "" }),
            ],
            &first,
            &first,
        );
        assert!(is_included_asset(&first, &at(&first, "src/a.splat")));

        // A second app whose config names nothing leaves the first's filter.
        install_assets_include(&[], &second, &second);
        assert!(
            is_included_asset(&first, &at(&first, "src/a.splat")),
            "another root's empty config cleared this root's filter"
        );
        assert!(is_included_asset(&first, "/src/a.splat"), "a served url");
        // Neither the second app nor one that never installed a filter (an
        // app with no vite.config) is judged by the first app's patterns:
        // not its files, not its served urls.
        for other in [&second, &third] {
            for path in [
                at(other, "src/a.pointcloud"),
                at(other, "src/a.splat"),
                "/src/a.pointcloud".to_string(),
                "/src/a.splat".to_string(),
            ] {
                assert!(
                    !is_included_asset(other, &path),
                    "the first app's patterns named {path} for another app"
                );
            }
        }
        // The first app's own patterns still reach a file anywhere, another
        // app's directory included: it is the asking app that matters.
        assert!(is_included_asset(&first, &at(&second, "src/a.splat")));

        // The second app names its own type: each root answers for itself.
        install_assets_include(&[serde_json::json!("**/*.voxels")], &second, &second);
        assert!(is_included_asset(&second, &at(&second, "src/a.voxels")));
        assert!(!is_included_asset(&first, &at(&first, "src/a.voxels")));
        assert!(!is_included_asset(&second, &at(&second, "src/a.splat")));

        // A root's own config replaces its filter, and only its own; the
        // root's other spelling (as it resolves) is the same app.
        let real = std::fs::canonicalize(&first).unwrap();
        assert!(is_included_asset(&real, &at(&real, "src/a.splat")));
        install_assets_include(&[], &real, &real);
        assert!(!is_included_asset(&first, &at(&first, "src/a.splat")));
        assert!(!is_included_asset(&real, &at(&real, "src/a.splat")));
        assert!(is_included_asset(&second, &at(&second, "src/a.voxels")));
        install_assets_include(&[], &second, &second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The root through a symlink and as it resolves are one app: a glob
    // resolved against the start directory (the system's spelling) names a
    // file or a served url asked for under the other spelling, as Vite,
    // whose ids are real paths, would.
    #[cfg(unix)]
    #[test]
    fn a_root_reached_through_a_symlink_is_the_same_app() {
        let dir =
            std::env::temp_dir().join(format!("oj-assets-include-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real/models")).unwrap();
        let real = std::fs::canonicalize(dir.join("real")).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let at = |root: &std::path::Path, rel: &str| root.join(rel).to_string_lossy().into_owned();

        // Installed under the symlink, started in the real directory.
        install_assets_include(&[serde_json::json!("models/*.obj")], &link, &real);
        for root in [&link, &real] {
            assert!(is_included_asset(root, &at(&real, "models/x.obj")));
            assert!(
                is_included_asset(root, &at(&link, "models/x.obj")),
                "a file named through the symlink"
            );
            assert!(is_included_asset(root, "/models/x.obj"), "a served url");
            assert!(!is_included_asset(root, &at(&link, "src/models/x.obj")));
        }
        install_assets_include(&[], &link, &real);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mime_is_case_insensitive_with_octet_stream_fallback() {
        assert_eq!(asset_mime("SVG"), "image/svg+xml");
        assert_eq!(asset_mime("jfif"), "image/jpeg");
        assert_eq!(asset_mime("webmanifest"), "application/manifest+json");
        assert_eq!(asset_mime("xyz"), "application/octet-stream");
    }
}
