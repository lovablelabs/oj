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
/// RegExp is tested against the file's absolute path. One per server, built
/// from its resolved config ([`AssetsInclude::new`]) and handed to every
/// check, as Vite's resolved config carries the one function every plugin
/// consults: two servers in one process (the dev tests run many) never
/// share or overwrite each other's.
#[derive(Debug)]
pub struct AssetsInclude {
    root: std::path::PathBuf,
    globs: globset::GlobSet,
    regexes: Vec<regex::Regex>,
}

impl AssetsInclude {
    /// The filter for a config's `assetsInclude`: string globs and
    /// `{ "regex": source, "flags": flags }` objects, as the config
    /// extraction carries them. `None` for an empty list. A pattern that
    /// does not parse is skipped, as picomatch would never have matched it.
    pub fn new(patterns: &[serde_json::Value], root: &std::path::Path) -> Option<Self> {
        if patterns.is_empty() {
            return None;
        }
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
        Some(AssetsInclude {
            root: root.to_path_buf(),
            globs: globs.build().unwrap_or_else(|_| globset::GlobSet::empty()),
            regexes,
        })
    }

    /// Whether the config's `assetsInclude` names this file (query and hash
    /// ignored). A served url's root-relative path (`/src/x.gltf`, no file
    /// at the filesystem root) is tried under the root too, so a glob
    /// resolved against the root matches it as well; an absolute path that
    /// is a file elsewhere is judged as it is, as Vite judges the file's own
    /// path.
    pub fn is_match(&self, path: &str) -> bool {
        let clean = path
            .split(['?', '#'])
            .next()
            .unwrap_or(path)
            .replace('\\', "/");
        if clean.is_empty() || clean.starts_with('\0') {
            return false;
        }
        let matches =
            |p: &str| self.globs.is_match(p) || self.regexes.iter().any(|re| re.is_match(p));
        if matches(&clean) {
            return true;
        }
        let as_path = std::path::Path::new(&clean);
        if as_path.starts_with(&self.root) || as_path.exists() {
            return false;
        }
        let under_root = self.root.join(clean.trim_start_matches('/'));
        matches(&under_root.to_string_lossy().replace('\\', "/"))
    }
}

/// Whether a server's `assetsInclude` (none: no config names any) names
/// this file.
pub fn is_included_asset(include: Option<&AssetsInclude>, path: &str) -> bool {
    include.is_some_and(|i| i.is_match(path))
}

/// Whether a path or url is an asset: a known type by its extension, or a
/// file the server's `assetsInclude` names.
pub fn is_asset_path(include: Option<&AssetsInclude>, path: &str) -> bool {
    is_asset_url(path) || is_included_asset(include, path)
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
    // root, a file that exists elsewhere is not. The install is one per
    // process, so this is the one test that touches it.
    #[test]
    fn configured_assets_include_names_more_files() {
        let dir = std::env::temp_dir().join(format!("oj-assets-include-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("app");
        let outside = dir.join("elsewhere");
        for d in ["src/deep", "models", "src/models", "a"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::create_dir_all(&outside).unwrap();
        for f in [
            "src/scene.gltf",
            "src/deep/scene.gltf",
            "models/x.obj",
            "src/models/x.obj",
            "a/b.bin",
            "src/scene.png",
        ] {
            std::fs::write(root.join(f), b"").unwrap();
        }
        std::fs::write(outside.join("x.gltf"), b"").unwrap();
        let under = |rel: &str| root.join(rel).to_string_lossy().into_owned();
        let include = AssetsInclude::new(
            &[
                serde_json::json!("**/*.gltf"),
                serde_json::json!("./models/*.obj"),
                serde_json::json!({ "regex": "\\.BIN$", "flags": "i" }),
            ],
            &root,
        )
        .expect("patterns make a filter");
        let some = Some(&include);
        assert!(is_included_asset(some, &under("src/scene.gltf")));
        assert!(is_included_asset(
            some,
            &format!("{}?url", under("src/deep/scene.gltf"))
        ));
        assert!(
            is_included_asset(some, "/src/deep/scene.gltf"),
            "a served url's path is tried under the root"
        );
        assert!(
            is_included_asset(some, &under("models/x.obj")),
            "a `./` pattern"
        );
        assert!(
            !is_included_asset(some, &under("src/models/x.obj")),
            "`*` stops at `/`, as picomatch's does"
        );
        assert!(
            is_included_asset(some, &under("a/b.bin")),
            "the RegExp, case-insensitive"
        );
        assert!(!is_included_asset(some, &under("src/scene.png")));
        assert!(!is_included_asset(some, "\0virtual:scene.gltf"));
        assert!(
            !is_included_asset(some, &outside.join("x.gltf").to_string_lossy()),
            "a file outside the root is judged by its own path"
        );
        assert!(is_asset_path(some, &under("src/scene.gltf")));
        assert!(is_asset_path(some, &under("src/photo.PNG")));
        assert!(
            !is_included_asset(None, &under("src/scene.gltf")),
            "no filter"
        );
        assert!(
            AssetsInclude::new(&[], &root).is_none(),
            "an empty list is no filter"
        );
        // Two filters stand apart: one server's never answers for another.
        let other = AssetsInclude::new(&[serde_json::json!("**/*.obj")], &root).unwrap();
        assert!(!is_included_asset(Some(&other), &under("src/scene.gltf")));
        assert!(is_included_asset(Some(&other), &under("src/models/x.obj")));
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
