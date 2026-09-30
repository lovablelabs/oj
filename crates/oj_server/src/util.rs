use super::*;

pub(crate) fn bytes_to_string(bytes: Vec<u8>) -> std::io::Result<String> {
    match simdutf8::basic::from_utf8(&bytes) {
        Ok(_) => Ok(unsafe { String::from_utf8_unchecked(bytes) }),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        )),
    }
}

pub(crate) fn hex_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    out
}

pub(crate) fn hex_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    String::from_utf8(out).ok()
}

// oj hex-encodes its own /@id/ links, but a Vite plugin ships raw /@id/<id> URLs
// (\0 as __x00__): decode hex when valid, else fall back so both forms resolve.
pub(crate) fn decode_at_id(seg: &str) -> String {
    if let Some(s) = hex_decode(seg) {
        return s;
    }
    urldecode(seg).replace("__x00__", "\0")
}

pub(crate) fn is_asset_ext(ext: &str) -> bool {
    // A plain `.wasm` import is served as a URL module (Vite asks for `?init`).
    oj_compiler::assets::is_asset_ext(ext) || ext.eq_ignore_ascii_case("wasm")
}

pub(crate) fn is_asset_path(file: &Path) -> bool {
    file.extension()
        .and_then(|e| e.to_str())
        .map(is_asset_ext)
        .unwrap_or(false)
}

pub(crate) fn content_type(ext: &str) -> &'static str {
    match ext {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript",
        "css" => "text/css",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "txt" | "map2" => "text/plain; charset=utf-8",
        // Any other known asset type (case-insensitive), else octet-stream.
        other => oj_compiler::assets::asset_mime(other),
    }
}

pub(crate) fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

// The single gate for serving an absolute (`/@fs`) path: the CANONICAL path must be
// inside an allowed root and not denied (raw `starts_with` collapses neither `..` nor
// symlinks); uncanonicalizable paths are refused. Returns the ORIGINAL candidate so a
// `preserveSymlinks` caller keeps the module identity it asked for.
pub(crate) fn fs_gate(state: &ServerState, candidate: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(candidate).ok()?;
    // Vite's isFileLoadingAllowed: `server.fs.strict: false` skips the allow
    // list entirely (the deny list below still applies). Roots are stored raw
    // AND canonical (insert_allow_root), so this is hash lookups per ancestor.
    let allowed = !state.fs_strict || {
        let allow = state.fs_allow.lock().unwrap();
        real.ancestors().any(|a| allow.contains(a))
    };
    if !allowed || path_is_denied(&real, &state.root, &state.fs_deny) {
        return None;
    }
    Some(candidate.to_path_buf())
}

/// Allow roots are inserted raw AND canonicalized (Vite resolves `fs.allow` up
/// front), so `fs_gate` never canonicalizes roots per request. A root whose
/// symlink is retargeted mid-session keeps its old canonical entry until restart.
pub(crate) fn insert_allow_root(allow: &mut std::collections::HashSet<PathBuf>, root: PathBuf) {
    if let Ok(real) = std::fs::canonicalize(&root) {
        if real != root {
            allow.insert(real);
        }
    }
    allow.insert(root);
}

pub(crate) fn allow_root(set: &Mutex<std::collections::HashSet<PathBuf>>, root: PathBuf) {
    insert_allow_root(&mut set.lock().unwrap(), root);
}

/// Successful canonicalizations memoized on the server: plugin watch lists
/// re-canonicalize hundreds of stable paths per save. Failures retry (a watched
/// file can appear later); a retargeted symlink keeps its first resolution.
pub(crate) fn canonicalize_memo(state: &ServerState, p: &str) -> PathBuf {
    if let Some(hit) = state.canon_memo.lock().unwrap().get(p) {
        return hit.clone();
    }
    match std::fs::canonicalize(p) {
        Ok(real) => {
            state
                .canon_memo
                .lock()
                .unwrap()
                .insert(p.to_string(), real.clone());
            real
        }
        Err(_) => PathBuf::from(p),
    }
}

pub(crate) fn urldecode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
