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

    #[test]
    fn mime_is_case_insensitive_with_octet_stream_fallback() {
        assert_eq!(asset_mime("SVG"), "image/svg+xml");
        assert_eq!(asset_mime("jfif"), "image/jpeg");
        assert_eq!(asset_mime("webmanifest"), "application/manifest+json");
        assert_eq!(asset_mime("xyz"), "application/octet-stream");
    }
}
