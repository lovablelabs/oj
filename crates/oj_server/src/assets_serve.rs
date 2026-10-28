use super::*;

pub(crate) fn is_worker_query(url: &str) -> bool {
    match url.split_once('?') {
        Some((_, q)) => q
            .split('&')
            .any(|kv| kv == "worker" || kv == "sharedworker"),
        None => false,
    }
}

pub(crate) fn query_asset_kind(query: Option<&str>) -> Option<&'static str> {
    let q = query?;
    let has = |kind| q.split('&').any(|kv| kv == kind);
    // Vite's vite:asset reads `raw` before `url`; vite:worker loads before it.
    if has("raw") && !has("worker") && !has("sharedworker") {
        return Some("raw");
    }
    for kind in ["url", "worker", "sharedworker", "raw", "inline", "init"] {
        if q.split('&').any(|kv| kv == kind) {
            return Some(kind);
        }
    }
    // `?no-inline` only differs from `?url` in the build (never inlined).
    if q.split('&').any(|kv| kv == "no-inline") {
        return Some("url");
    }
    None
}

/// The url a `?url` module exports: the file's, keeping the request's HMR
/// `t=` stamp so an updated asset gets a url its `<img>` refetches (Vite).
pub(crate) fn asset_export_url(url: &str) -> String {
    let (clean, query) = url.split_once('?').unwrap_or((url, ""));
    match query.split('&').find(|kv| kv.starts_with("t=")) {
        Some(t) => format!("{clean}?{t}"),
        None => clean.to_string(),
    }
}

/// What Vite's dev `fileToDevUrl` (plugins/asset.ts) needs besides the file:
/// the prefix every URL takes (`server.origin` joined with the base) and
/// `build.assetsInlineLimit`, which decides when an svg is inlined.
#[derive(Clone, Debug)]
pub(crate) struct DevAssetUrls {
    /// No trailing slash; empty for no origin and a `/` base.
    pub(crate) prefix: String,
    pub(crate) inline_limit: u64,
}

impl Default for DevAssetUrls {
    fn default() -> Self {
        Self {
            prefix: String::new(),
            // Vite's DEFAULT_ASSETS_INLINE_LIMIT.
            inline_limit: 4096,
        }
    }
}

impl DevAssetUrls {
    pub(crate) fn new(origin: Option<&str>, base: Option<&str>, inline_limit: Option<u64>) -> Self {
        let origin = origin.unwrap_or("").trim_end_matches('/');
        let base = crate::start_dev_url_base(base.unwrap_or("/"));
        Self {
            prefix: format!("{origin}{base}"),
            inline_limit: inline_limit.unwrap_or(4096),
        }
    }

    /// `url` (root-relative) under the prefix.
    pub(crate) fn join(&self, url: &str) -> String {
        format!("{}{url}", self.prefix)
    }

    /// Vite's dev fileToDevUrl for `file` requested as `url`: a small enough
    /// svg inlines as a data URL (shouldInline), anything else is its URL
    /// under the prefix, keeping an HMR `t=` stamp.
    pub(crate) fn file_url(&self, file: &Path, url: &str) -> String {
        if file.extension().is_some_and(|e| e == "svg") {
            if let Ok(content) = std::fs::read(file) {
                if self.should_inline(url, &content) {
                    return svg_to_data_url(&content);
                }
            }
        }
        self.join(&asset_export_url(url))
    }

    /// Vite's dev shouldInline: `?no-inline` never, `?inline` always, an svg
    /// with a fragment never, else under the limit unless a Git LFS pointer.
    fn should_inline(&self, url: &str, content: &[u8]) -> bool {
        let query = url.split_once('?').map_or("", |(_, q)| q);
        let has = |kind: &str| {
            query
                .split('&')
                .any(|kv| kv == kind || kv.starts_with(&format!("{kind}=")))
        };
        if has("no-inline") {
            return false;
        }
        if has("inline") {
            return true;
        }
        if url.contains('#') {
            return false;
        }
        (content.len() as u64) < self.inline_limit
            && !content.starts_with(b"version https://git-lfs.github.com")
    }
}

/// Vite's assetToDataURL: an svg as Vite's svgToDataURL encodes it, anything
/// else base64 under its MIME type.
pub(crate) fn asset_data_url(file: &Path, content: &[u8]) -> String {
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext == "svg" {
        return svg_to_data_url(content);
    }
    let mime = content_type(ext)
        .split(';')
        .next()
        .unwrap_or("application/octet-stream");
    format!("data:{mime};base64,{}", base64_encode(content))
}

/// Vite's svgToDataURL: URL-encoded markup, or base64 when the svg holds text,
/// a foreignObject or nested quotes, where the rewrite would be unsafe.
pub(crate) fn svg_to_data_url(content: &[u8]) -> String {
    let text = String::from_utf8_lossy(content);
    static NESTED_QUOTES: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#""[^"']*'[^"]*"|'[^'"]*"[^']*'"#).unwrap());
    if text.contains("<text") || text.contains("<foreignObject") || NESTED_QUOTES.is_match(&text) {
        return format!("data:image/svg+xml;base64,{}", base64_encode(content));
    }
    static BETWEEN_TAGS: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r">\s+<").unwrap());
    static WHITESPACE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\s+").unwrap());
    let packed = BETWEEN_TAGS.replace_all(text.trim(), "><");
    let encoded = packed
        .replace('"', "'")
        .replace('%', "%25")
        .replace('#', "%23")
        .replace('<', "%3c")
        .replace('>', "%3e");
    format!(
        "data:image/svg+xml,{}",
        WHITESPACE.replace_all(&encoded, "%20")
    )
}

fn js_default(value: String) -> String {
    format!("export default {};\n", serde_json::Value::String(value))
}

pub(crate) async fn asset_module(
    file: &Path,
    url: &str,
    kind: &str,
    urls: &DevAssetUrls,
) -> Result<String, String> {
    let clean_url = url.split('?').next().unwrap_or(url);
    match kind {
        "url" => {
            // A stylesheet's URL must serve its COMPILED css (Sass, PostCSS, ...)
            // as text; `?direct` is the plain-CSS request, as in Vite.
            let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
            if is_style_ext(ext) {
                return Ok(js_default(urls.join(&format!("{clean_url}?direct"))));
            }
            Ok(js_default(urls.file_url(file, url)))
        }
        "raw" => {
            let text = tokio::fs::read_to_string(file)
                .await
                .map_err(|e| format!("read {}: {e}", file.display()))?;
            Ok(js_default(text))
        }
        "inline" => {
            // A stylesheet's `?inline` goes through `inline_css_module` instead.
            let bytes = tokio::fs::read(file).await.map_err(|e| format!("read: {e}"))?;
            Ok(js_default(asset_data_url(file, &bytes)))
        }
        "worker" | "sharedworker" => {
            let ctor = if kind == "sharedworker" { "SharedWorker" } else { "Worker" };
            Ok(format!(
                "export default function () {{ return new {ctor}({clean_url:?}, {{ type: \"module\" }}); }}\n"
            ))
        }
        "init" => Ok(format!(
            "export default (imports = {{}}) => {{\n  const url = {clean_url:?};\n  const inst = (r) => r.instance;\n  const fallback = () => fetch(url).then((r) => r.arrayBuffer()).then((b) => WebAssembly.instantiate(b, imports)).then(inst);\n  if (WebAssembly.instantiateStreaming) {{\n    return WebAssembly.instantiateStreaming(fetch(url), imports).then(inst).catch(fallback);\n  }}\n  return fallback();\n}};\n"
        )),
        _ => Err(format!("unknown asset query: {kind}")),
    }
}

/// `?inline`: the compiled stylesheet as a string, output of the same pipeline a
/// plain css import runs (Vite parity); a CSS module inlines css, not its class map.
pub(crate) async fn inline_css_module(
    state: &Arc<ServerState>,
    file: &Path,
    url: &str,
) -> Result<String, String> {
    let clean = url.split('?').next().unwrap_or(url);
    let url = format!("{clean}?inline");
    let (_, module) = Box::pin(ensure_module(state, file, &url)).await?;
    let code = format!(
        "export default {};\n",
        serde_json::to_string(&module.code).unwrap()
    );
    let id = format!("{}?inline", file.display());
    css_post(state, file, &url, &id, code).await
}

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
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

pub(crate) fn is_server_module(file: &Path) -> bool {
    file.file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            [".server.ts", ".server.tsx", ".server.js", ".server.jsx"]
                .iter()
                .any(|s| n.ends_with(s))
        })
        .unwrap_or(false)
}

pub(crate) fn server_fn_stub(exports: &[String], url: &str) -> String {
    let mut out = String::from("import { __ojServerCall } from \"/@oj/server-fn.js\";\n");
    for name in exports {
        if name == "default" {
            out.push_str(&format!(
                "export default (...a) => __ojServerCall({url:?}, \"default\", a);\n"
            ));
        } else {
            out.push_str(&format!(
                "export const {name} = (...a) => __ojServerCall({url:?}, {name:?}, a);\n"
            ));
        }
    }
    out
}
