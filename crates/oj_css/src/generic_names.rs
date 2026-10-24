// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A string `css.modules.generateScopedName` as postcss-modules applies it:
//! generic-names 4 over loader-utils 3's interpolateName and getHashDigest.
//! `[local]` goes in first; `[hash]` / `[contenthash]` (`[algo:hash:digest:len]`)
//! hash `hashPrefix + relative path + "\0" + local`; then `[ext]`, `[name]`,
//! `[path]`, `[folder]` and `[query]`; characters outside
//! `[a-zA-Z0-9\-_ -￿]` become `-`, and a leading digit (or `--`)
//! gets a `_`.

use std::path::Path;
use std::sync::LazyLock;

use base64::Engine as _;
use regex::Regex;

static HASH_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\[(?:([^\[:\]]+):)?(?:hash|contenthash)(?::([a-z]+\d*(?:safe)?))?(?::(\d+))?\]",
    )
    .expect("static regex")
});

/// One module file's names under a pattern.
pub(crate) struct GenericNames {
    pattern: String,
    hash_prefix: String,
    /// The file relative to the context, `/`-separated.
    relative: String,
    ext: String,
    basename: String,
    directory: String,
    folder: String,
}

impl GenericNames {
    /// `file`: the module's absolute path; `context`: postcss-modules'
    /// (`process.cwd()`, the app root when Vite runs from it).
    pub(crate) fn new(
        pattern: &str,
        file: &Path,
        context: &Path,
        hash_prefix: Option<&str>,
    ) -> Self {
        let relative = relative_path(context, file);
        let ext = file
            .extension()
            .and_then(|e| e.to_str())
            .map_or_else(|| "bin".to_string(), str::to_string);
        let (basename, directory) = match file.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => {
                let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                // `path.relative(context, dir + "/_")` minus the `_`, with
                // `..` segments made `_`.
                let rel = relative_path(context, &dir.join("_"));
                let rel = rel.strip_suffix('_').unwrap_or(&rel).to_string();
                (stem.to_string(), escape_parent_segments(&rel))
            }
            _ => ("file".to_string(), String::new()),
        };
        let (directory, folder) = if directory.chars().count() <= 1 {
            (String::new(), String::new())
        } else {
            let folder = directory
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string();
            (directory, folder)
        };
        Self {
            pattern: pattern.to_string(),
            hash_prefix: hash_prefix.unwrap_or("").to_string(),
            relative,
            ext,
            basename,
            directory,
            folder,
        }
    }

    pub(crate) fn name(&self, local: &str) -> String {
        let named = replace_ci(&self.pattern, "[local]", local);
        let content = format!("{}{}\0{local}", self.hash_prefix, self.relative);
        let hashed = HASH_TOKEN.replace_all(&named, |c: &regex::Captures<'_>| {
            let max = c.get(3).and_then(|m| m.as_str().parse::<usize>().ok());
            hash_digest(
                content.as_bytes(),
                c.get(1).map(|m| m.as_str()),
                c.get(2).map(|m| m.as_str()),
                max,
            )
        });
        let mut url = hashed.into_owned();
        for (token, value) in [
            ("[ext]", self.ext.as_str()),
            ("[name]", self.basename.as_str()),
            ("[path]", self.directory.as_str()),
            ("[folder]", self.folder.as_str()),
            ("[query]", ""),
        ] {
            url = replace_ci(&url, token, value);
        }
        let mut out: String = url
            .chars()
            .map(|c| {
                let allowed = c.is_ascii_alphanumeric()
                    || c == '-'
                    || c == '_'
                    || ('\u{00A0}'..='\u{FFFF}').contains(&c);
                if allowed {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        // `/^((-?[0-9])|--)/` -> `_$1`.
        let bytes = out.as_bytes();
        let leading = bytes.first().is_some_and(u8::is_ascii_digit)
            || (bytes.first() == Some(&b'-')
                && bytes
                    .get(1)
                    .is_some_and(|b| b.is_ascii_digit() || *b == b'-'));
        if leading {
            out.insert(0, '_');
        }
        out
    }
}

/// `path.relative(from, to)` with `/` separators.
fn relative_path(from: &Path, to: &Path) -> String {
    let from: Vec<_> = from.components().collect();
    let to_c: Vec<_> = to.components().collect();
    let common = from.iter().zip(&to_c).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_string(); from.len() - common];
    parts.extend(
        to_c[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    parts.join("/")
}

/// `.replace(/\.\.(\/)?/g, "_$1")`.
fn escape_parent_segments(s: &str) -> String {
    s.replace("..", "_")
}

/// `str.replace(/token/gi, value)` for a literal bracketed token.
fn replace_ci(s: &str, token: &str, value: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (at, _) in lower.match_indices(token) {
        out.push_str(&s[last..at]);
        out.push_str(value);
        last = at + token.len();
    }
    out.push_str(&s[last..]);
    out
}

/// loader-utils' getHashDigest: xxhash64 unless the token names another
/// algorithm; `hex` / `base64` / `base64url` digests, or the `baseNN` and
/// `base64safe` encodings of the digest bytes; then cut to `max`.
fn hash_digest(
    content: &[u8],
    algorithm: Option<&str>,
    digest: Option<&str>,
    max: Option<usize>,
) -> String {
    let bytes = match algorithm
        .unwrap_or("xxhash64")
        .to_ascii_lowercase()
        .as_str()
    {
        "md4" | "native-md4" => {
            use md4::Digest as _;
            md4::Md4::digest(content).to_vec()
        }
        "md5" => {
            use md5::Digest as _;
            md5::Md5::digest(content).to_vec()
        }
        "sha1" => {
            use sha1::Digest as _;
            sha1::Sha1::digest(content).to_vec()
        }
        "sha256" => {
            use sha2::Digest as _;
            sha2::Sha256::digest(content).to_vec()
        }
        "sha512" => {
            use sha2::Digest as _;
            sha2::Sha512::digest(content).to_vec()
        }
        other => {
            if other != "xxhash64" {
                eprintln!("oj: css.modules.generateScopedName hash {other:?} is not supported; using xxhash64");
            }
            xxhash_rust::xxh64::xxh64(content, 0).to_be_bytes().to_vec()
        }
    };
    let max = max.unwrap_or(9999);
    let digest = digest.unwrap_or("hex");
    let base = match digest {
        "base64safe" => Some(64),
        d => d
            .strip_prefix("base")
            .and_then(|n| n.parse::<u32>().ok())
            .filter(|n| [26, 32, 36, 49, 52, 58, 62].contains(n)),
    };
    if let Some(base) = base {
        return encode_to_base(&bytes, base, max);
    }
    let text = match digest {
        "base64" => base64::engine::general_purpose::STANDARD.encode(&bytes),
        "base64url" => base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes),
        _ => bytes.iter().map(|b| format!("{b:02x}")).collect(),
    };
    text.chars().take(max).collect()
}

/// loader-utils' encodeBufferToBase: the digest read as a little-endian
/// number of 32-bit words, its lowest `length` digits, most significant first.
fn encode_to_base(bytes: &[u8], base: u32, length: usize) -> String {
    let table: &[u8] = match base {
        26 => b"abcdefghijklmnopqrstuvwxyz",
        32 => b"123456789abcdefghjkmnpqrstuvwxyz",
        36 => b"0123456789abcdefghijklmnopqrstuvwxyz",
        49 => b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ",
        52 => b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ",
        58 => b"123456789abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ",
        62 => b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ",
        _ => b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ-_",
    };
    let limit = ((bytes.len() * 8) as f64 / f64::from(base).log2()).ceil() as usize;
    let length = length.min(limit);
    let mut words: Vec<u32> = bytes
        .chunks(4)
        .map(|c| {
            let mut w = [0u8; 4];
            w[..c.len()].copy_from_slice(c);
            u32::from_le_bytes(w)
        })
        .collect();
    let mut out = Vec::with_capacity(length);
    for _ in 0..length {
        let mut carry: u64 = 0;
        for w in words.iter_mut().rev() {
            let value = carry * 4_294_967_296 + u64::from(*w);
            carry = value % u64::from(base);
            *w = (value / u64::from(base)) as u32;
        }
        out.push(table[carry as usize]);
    }
    out.reverse();
    String::from_utf8(out).expect("ascii table")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Recorded from Vite 8.3.5 (postcss-modules 9, generic-names 4,
    // loader-utils 3), building from the app root: src/mod.module.css with
    // `.button` and `.apply-color`, src/sub/deep.module.css with `.x1`.
    #[test]
    fn names_match_vites_generic_names() {
        let root = Path::new("/app");
        let names = |pattern: &str, file: &str, local: &str| {
            GenericNames::new(pattern, &root.join(file), root, None).name(local)
        };
        let cases: &[(&str, [&str; 3])] = &[
            (
                "[name]__[local]___[hash:base64:5]",
                [
                    "mod-module__button___7La1-",
                    "mod-module__apply-color___TGXoe",
                    "deep-module__x1___o5wzR",
                ],
            ),
            (
                "[local]_[hash:hex:8]",
                ["button_ecb6b5f8", "apply-color_4c65e87a", "x1_a39c3347"],
            ),
            (
                "[path][name]__[local]",
                [
                    "src-mod-module__button",
                    "src-mod-module__apply-color",
                    "src-sub-deep-module__x1",
                ],
            ),
            (
                "[folder]-[local]-[contenthash:base64:6]",
                [
                    "src-button-7La1-D",
                    "src-apply-color-TGXoer",
                    "sub-x1-o5wzRy",
                ],
            ),
            (
                "[sha1:hash:hex:7]_[local]",
                ["_0579dfe_button", "_317c8af_apply-color", "_31b6e35_x1"],
            ),
            (
                "[md5:hash:base62:6]-[local]",
                ["jF3F3z-button", "OSXQWT-apply-color", "djMlZk-x1"],
            ),
            (
                "[hash:base64]__[local]",
                [
                    "_7La1-DEPqlc-__button",
                    "TGXoerfQgg8-__apply-color",
                    "o5wzRy6C6JE-__x1",
                ],
            ),
        ];
        for (pattern, [button, apply, x1]) in cases {
            assert_eq!(
                names(pattern, "src/mod.module.css", "button"),
                *button,
                "{pattern}"
            );
            assert_eq!(
                names(pattern, "src/mod.module.css", "apply-color"),
                *apply,
                "{pattern}"
            );
            assert_eq!(
                names(pattern, "src/sub/deep.module.css", "x1"),
                *x1,
                "{pattern}"
            );
        }
    }
}
