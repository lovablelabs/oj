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
/// import is their URL (`config.assetsInclude(file)` in Vite, the
/// `DEFAULT_ASSETS_RE` test or this filter). A string is a glob resolved
/// against the root, as `createFilter` resolves one against its base; a
/// RegExp is tested against the file's absolute path. Installed from the
/// resolved config ([`install_assets_include`]), as Vite's resolved config
/// carries the one function every plugin consults: one filter per app root,
/// so a process that loads several apps' configs (the dev server's tests
/// boot many servers side by side) keeps each app's own.
struct AssetsInclude {
    root: std::path::PathBuf,
    globs: globset::GlobSet,
    regexes: Vec<regex::Regex>,
}

impl AssetsInclude {
    fn matches(&self, path: &str) -> bool {
        self.globs.is_match(path) || self.regexes.iter().any(|re| re.is_match(path))
    }

    fn names_nothing(&self) -> bool {
        self.globs.is_empty() && self.regexes.is_empty()
    }
}

/// Each root's filter, the last installed for it. A root whose config names
/// nothing keeps an empty one: its files are its own, never another root's.
static ASSETS_INCLUDE: std::sync::RwLock<Vec<AssetsInclude>> = std::sync::RwLock::new(Vec::new());

/// Installs `root`'s `assetsInclude` for [`is_included_asset`]: string
/// globs and `{ "regex": source, "flags": flags }` objects, as the config
/// extraction carries them. An empty list names nothing under `root`; it
/// leaves every other root's filter as it is. A pattern that does not parse
/// is skipped, as picomatch would never have matched it.
pub fn install_assets_include(patterns: &[serde_json::Value], root: &std::path::Path) {
    let installed = {
        let mut globs = globset::GlobSetBuilder::new();
        let mut regexes = Vec::new();
        let root_slash = root.to_string_lossy().replace('\\', "/");
        for p in patterns {
            match p {
                serde_json::Value::String(glob) => {
                    // createFilter resolves a relative pattern against its
                    // base (`**/*.gltf` is `<root>/**/*.gltf`); an absolute
                    // one stands.
                    let absolute = if glob.starts_with('/') {
                        glob.clone()
                    } else {
                        format!(
                            "{}/{}",
                            root_slash.trim_end_matches('/'),
                            glob.trim_start_matches("./")
                        )
                    };
                    // picomatch: `*` stops at `/`, `**` crosses directories,
                    // and createFilter matches dotfiles (`dot: true`).
                    if let Ok(g) = globset::GlobBuilder::new(&absolute)
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
        AssetsInclude {
            root: root.to_path_buf(),
            globs: globs.build().unwrap_or_else(|_| globset::GlobSet::empty()),
            regexes,
        }
    };
    let mut filters = ASSETS_INCLUDE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match filters.iter_mut().find(|f| f.root == root) {
        Some(slot) => *slot = installed,
        None => filters.push(installed),
    }
}

/// Whether a config's `assetsInclude` names this file (query and hash
/// ignored). A file under an installed root is judged by that root's filter
/// alone, as Vite judges a file by its own config. Any other path is tried
/// against each: a served url's root-relative path (`/src/x.gltf`, no file
/// at the filesystem root) under the root too, so a glob resolved against
/// the root matches it as well; an absolute path that is a file elsewhere
/// as it is, as Vite judges the file's own path.
pub fn is_included_asset(path: &str) -> bool {
    let filters = ASSETS_INCLUDE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if filters.iter().all(AssetsInclude::names_nothing) {
        return false;
    }
    let clean = path
        .split(['?', '#'])
        .next()
        .unwrap_or(path)
        .replace('\\', "/");
    if clean.is_empty() || clean.starts_with('\0') {
        return false;
    }
    let as_path = std::path::Path::new(&clean);
    // The innermost installed root the file is under owns it.
    let owner = filters
        .iter()
        .filter(|f| as_path.starts_with(&f.root))
        .max_by_key(|f| f.root.as_os_str().len());
    if let Some(owner) = owner {
        return owner.matches(&clean);
    }
    if filters.iter().any(|f| f.matches(&clean)) {
        return true;
    }
    if as_path.exists() {
        return false;
    }
    filters.iter().any(|f| {
        let under_root = f.root.join(clean.trim_start_matches('/'));
        f.matches(&under_root.to_string_lossy().replace('\\', "/"))
    })
}

/// Whether a path or url is an asset: a known type by its extension, or a
/// file the config's `assetsInclude` names.
pub fn is_asset_path(path: &str) -> bool {
    is_asset_url(path) || is_included_asset(path)
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

    // Vite's `assetsInclude` beyond the known types: a glob resolved against
    // the root (createFilter's base), a RegExp with its flags against the
    // absolute path; a served url's root-relative path is tried under the
    // root, a file that exists elsewhere is not.
    #[test]
    fn configured_assets_include_names_more_files() {
        let dir = std::env::temp_dir().join(format!("oj-assets-include-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("app");
        let outside = dir.join("outside");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("x.gltf"), "x").unwrap();
        let under = |rel: &str| root.join(rel).to_string_lossy().replace('\\', "/");
        install_assets_include(
            &[
                serde_json::json!("**/*.gltf"),
                serde_json::json!("./models/*.obj"),
                serde_json::json!({ "regex": "\\.BIN$", "flags": "i" }),
                serde_json::json!(7),
            ],
            &root,
        );
        assert!(is_included_asset(&under("src/scene.gltf")));
        assert!(is_included_asset(&format!(
            "{}?url",
            under("src/scene.gltf")
        )));
        assert!(
            is_included_asset("/src/deep/scene.gltf"),
            "a served url's path"
        );
        assert!(is_included_asset(&under("models/x.obj")), "a `./` pattern");
        assert!(
            !is_included_asset(&under("src/models/x.obj")),
            "`*` stops at `/`"
        );
        assert!(
            is_included_asset(&under("a/b.bin")),
            "the RegExp's `i` flag"
        );
        assert!(!is_included_asset(&under("src/scene.png")));
        assert!(!is_included_asset("\0virtual:scene.gltf"));
        assert!(
            !is_included_asset(&outside.join("x.gltf").to_string_lossy()),
            "a file outside the root is judged by its own path, as Vite does"
        );
        assert!(is_asset_path(&under("src/scene.gltf")));
        assert!(is_asset_path(&under("src/photo.PNG")));
        install_assets_include(&[], &root);
        assert!(!is_included_asset(&under("src/scene.gltf")), "uninstalled");
        assert!(is_asset_path(&under("src/photo.PNG")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // One filter per root: a process that loads a second app's config (the
    // dev server's tests boot many servers in one process) leaves the first
    // app's filter as it is, and neither app's patterns name the other's
    // files. A config without `assetsInclude` used to clear the one filter
    // the process had, under a server still using it.
    #[test]
    fn each_root_keeps_its_own_assets_include() {
        let dir =
            std::env::temp_dir().join(format!("oj-assets-include-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (first, second) = (dir.join("first"), dir.join("second"));
        let at =
            |root: &std::path::Path, rel: &str| root.join(rel).to_string_lossy().replace('\\', "/");
        install_assets_include(
            &[
                serde_json::json!("**/*.splat"),
                serde_json::json!({ "regex": "\\.pointcloud$", "flags": "" }),
            ],
            &first,
        );
        assert!(is_included_asset(&at(&first, "src/a.splat")));

        // A second app whose config names nothing.
        install_assets_include(&[], &second);
        assert!(
            is_included_asset(&at(&first, "src/a.splat")),
            "another root's empty config cleared this root's filter"
        );
        assert!(
            is_included_asset("/src/roots-only/a.splat"),
            "a served url's path"
        );
        assert!(
            !is_included_asset(&at(&second, "src/a.pointcloud")),
            "the first root's RegExp named the second root's file"
        );
        assert!(is_included_asset(&at(&first, "src/a.pointcloud")));

        // The second app names its own type: each root answers for its files.
        install_assets_include(&[serde_json::json!("**/*.voxels")], &second);
        assert!(is_included_asset(&at(&second, "src/a.voxels")));
        assert!(!is_included_asset(&at(&first, "src/a.voxels")));
        assert!(is_included_asset(&at(&first, "src/a.splat")));
        assert!(!is_included_asset(&at(&second, "src/a.splat")));

        // A root's own config replaces its filter, and only its own.
        install_assets_include(&[], &first);
        assert!(!is_included_asset(&at(&first, "src/a.splat")));
        assert!(is_included_asset(&at(&second, "src/a.voxels")));
        install_assets_include(&[], &second);
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
