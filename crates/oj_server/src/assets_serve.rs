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
        // `inline` is Vite's `[?&]inline\b`; the others match whole.
        let hit = match kind {
            "inline" => has_word_param(&format!("?{q}"), "inline"),
            _ => q.split('&').any(|kv| kv == kind),
        };
        if hit {
            return Some(kind);
        }
    }
    // `?no-inline` only differs from `?url` in the build (never inlined).
    if q.split('&').any(|kv| kv == "no-inline") {
        return Some("url");
    }
    None
}

/// The url an asset module names: the request's, less `url` (Vite's
/// removeUrlQuery) and `import` (stripped before any plugin sees the id).
/// Every other parameter stays, an HMR `t=` stamp included, so an updated
/// asset gets a url its `<img>` refetches.
pub(crate) fn asset_export_url(url: &str) -> String {
    let (clean, query) = url.split_once('?').unwrap_or((url, ""));
    let kept: Vec<&str> = query
        .split('&')
        .filter(|kv| !kv.is_empty() && *kv != "url" && *kv != "import")
        .collect();
    if kept.is_empty() {
        clean.to_string()
    } else {
        format!("{clean}?{}", kept.join("&"))
    }
}

/// Whether `url`'s query has the bare parameter `name` (Vite's `[?&]name\b`).
fn has_param(url: &str, name: &str) -> bool {
    url.split_once('?').is_some_and(|(_, q)| {
        q.split('&')
            .any(|kv| kv == name || kv.strip_prefix(name).is_some_and(|r| r.starts_with('=')))
    })
}

/// Vite's encodeURIPath: `encodeURI` on the path, the query and fragment kept;
/// a data URL as is.
pub(crate) fn encode_uri_path(url: &str) -> String {
    if url.starts_with("data:") {
        return url.to_string();
    }
    let split = url.find(['?', '#']).unwrap_or(url.len());
    let (path, postfix) = url.split_at(split);
    let mut out = String::with_capacity(url.len());
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b";,/?:@&=+$-_.!~*'()#".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out + postfix
}

/// `decodeURI`: `%XX` escapes of reserved characters stay, as in JS.
pub(crate) fn decode_uri(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                let byte = (h * 16 + l) as u8;
                if !(byte.is_ascii() && b";/?:@&=+$,#".contains(&byte)) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// What Vite's dev `fileToDevUrl` (plugins/asset.ts) needs besides the file:
/// the prefix every URL takes (`server.origin` joined with the base) and
/// `build.assetsInlineLimit`, which decides when an svg is inlined.
#[derive(Clone, Debug)]
pub(crate) struct DevAssetUrls {
    /// `server.origin` and the decoded base (Vite's `decodedBase`), no
    /// trailing slash; empty for no origin and a `/` base.
    pub(crate) prefix: String,
    /// The same with the base as written: Vite's css url replacer joins a
    /// public file's URL onto `config.base`, not the decoded one.
    pub(crate) raw_prefix: String,
    pub(crate) inline_limit: u64,
    /// Environments whose own `build.assetsInlineLimit` is a number.
    pub(crate) env_limits: std::collections::BTreeMap<String, u64>,
    /// Environments whose `build.assetsInlineLimit` is a function, which the
    /// decider asks; with no entry for an environment, the top level's kind.
    pub(crate) env_functions: std::collections::BTreeSet<String>,
    /// Whether the top-level `build.assetsInlineLimit` is a function.
    pub(crate) top_function: bool,
    /// The plugin host's answer for a function-valued limit.
    pub(crate) decider: Option<InlineDecider>,
}

/// A function-valued `build.assetsInlineLimit` (kept in the plugin host),
/// asked per environment and file.
#[derive(Clone)]
pub(crate) struct InlineDecider(pub(crate) Arc<InlineFn>);

/// The environment's answer for `file`, or the error the function threw.
pub(crate) type InlineFn = dyn Fn(&str, &Path) -> Result<InlineRule, String> + Send + Sync;

/// What an environment's `build.assetsInlineLimit` says about one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InlineRule {
    /// The function's own answer.
    Decide(bool),
    /// A number (an environment's own limit, or the default after a
    /// function that answered nothing).
    Limit(u64),
}

impl std::fmt::Debug for InlineDecider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("assetsInlineLimit(fn)")
    }
}

impl Default for DevAssetUrls {
    fn default() -> Self {
        Self {
            prefix: String::new(),
            raw_prefix: String::new(),
            // Vite's DEFAULT_ASSETS_INLINE_LIMIT.
            inline_limit: 4096,
            env_limits: Default::default(),
            env_functions: Default::default(),
            top_function: false,
            decider: None,
        }
    }
}

impl DevAssetUrls {
    pub(crate) fn new(origin: Option<&str>, base: Option<&str>, inline_limit: Option<u64>) -> Self {
        let origin = origin.unwrap_or("").trim_end_matches('/');
        let base = crate::start_dev_url_base(base.unwrap_or("/"));
        Self {
            prefix: format!("{origin}{}", decode_uri(&base)),
            raw_prefix: format!("{origin}{base}"),
            inline_limit: inline_limit.unwrap_or(4096),
            ..Default::default()
        }
    }

    /// Records each environment's own `build.assetsInlineLimit` (`{ name: n
    /// | "fn" }`, as the extractor reports it) and whether the top level's is
    /// a function.
    pub(crate) fn with_environments(
        mut self,
        envs: Option<&serde_json::Value>,
        top_function: bool,
    ) -> Self {
        self.top_function = top_function;
        for (name, limit) in envs.and_then(|e| e.as_object()).into_iter().flatten() {
            match (limit.as_u64(), limit.as_str()) {
                (Some(n), _) => {
                    self.env_limits.insert(name.clone(), n);
                }
                (_, Some("fn")) => {
                    self.env_functions.insert(name.clone());
                }
                _ => {}
            }
        }
        self
    }

    /// `url` (root-relative) under the prefix.
    pub(crate) fn join(&self, url: &str) -> String {
        format!("{}{url}", self.prefix)
    }

    /// Vite's dev fileToDevUrl for `file` requested as `url` in environment
    /// `env`: `inline` in the query inlines any file, a small enough svg
    /// inlines (shouldInline), and anything else is its URL under the prefix
    /// (see [`asset_export_url`]). An error is a function `assetsInlineLimit`
    /// that threw, which fails the request as in Vite.
    pub(crate) fn file_url(&self, env: &str, file: &Path, url: &str) -> Result<String, String> {
        Ok(match self.inline_url(env, file, url)? {
            Some(data) => data,
            None => self.join(&asset_export_url(url)),
        })
    }

    /// fileToDevUrl's data URL for `file` requested as `url`, when it inlines.
    pub(crate) fn inline_url(
        &self,
        env: &str,
        file: &Path,
        url: &str,
    ) -> Result<Option<String>, String> {
        if has_word_param(url, "inline") {
            if let Ok(content) = std::fs::read(file) {
                return Ok(Some(asset_data_url(file, &content)));
            }
        }
        if file.extension().is_some_and(|e| e == "svg") {
            if let Ok(content) = std::fs::read(file) {
                if self.should_inline(env, file, url, &content)? {
                    return Ok(Some(svg_to_data_url(&content)));
                }
            }
        }
        Ok(None)
    }

    /// Vite's dev shouldInline: `?no-inline` never, `?inline` always, an svg
    /// with a fragment never, then a function `assetsInlineLimit`'s answer,
    /// else under the limit (the default one after a function) unless a Git
    /// LFS pointer.
    fn should_inline(
        &self,
        env: &str,
        file: &Path,
        url: &str,
        content: &[u8],
    ) -> Result<bool, String> {
        if has_word_param(url, "no-inline") {
            return Ok(false);
        }
        if has_word_param(url, "inline") {
            return Ok(true);
        }
        if url.contains('#') {
            return Ok(false);
        }
        // Vite reads `environment.config.build.assetsInlineLimit`: the
        // environment's own number, else a function the host answers, else
        // the top level's.
        let is_function = self.env_functions.contains(env)
            || (self.top_function && !self.env_limits.contains_key(env));
        let limit = match (&self.decider, self.env_limits.get(env)) {
            (Some(decider), _) if is_function => match (decider.0)(env, file)? {
                InlineRule::Decide(inline) => return Ok(inline),
                InlineRule::Limit(limit) => limit,
            },
            (_, Some(&own)) => own,
            _ => self.inline_limit,
        };
        Ok((content.len() as u64) < limit
            && !content.starts_with(b"version https://git-lfs.github.com"))
    }
}

/// Whether `url`'s query has `name` as Vite's `[?&]name\b` reads it: the
/// parameter starts with it and no word character follows (`inline`,
/// `inline=1`, `inline-x`, never `inlinex`).
pub(crate) fn has_word_param(url: &str, name: &str) -> bool {
    url.split_once('?').is_some_and(|(_, q)| {
        q.split('&').any(|kv| {
            kv.strip_prefix(name).is_some_and(|rest| {
                !rest
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            })
        })
    })
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

/// Vite's `/@vite/env` (client/env.ts): the client environment's user
/// `define` (`import.meta.env.*` left out, as clientInjections does), keys
/// sorted as serializeDefine sorts them, assigned onto the global object.
pub(crate) fn vite_env_module(config: &oj_config::OjConfig) -> String {
    let mut defines: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for (key, value) in oj_config::config_defines(config)
        .into_iter()
        .chain(oj_config::environment_defines(config, "client"))
    {
        if !key.starts_with("import.meta.env.") {
            defines.insert(key, value);
        }
    }
    let defines = defines
        .iter()
        .map(|(k, v)| format!("{}: {v}", serde_json::Value::String(k.clone())))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"const context = (() => {{
  if (typeof globalThis !== "undefined") {{
    return globalThis;
  }} else if (typeof self !== "undefined") {{
    return self;
  }} else if (typeof window !== "undefined") {{
    return window;
  }} else {{
    return Function("return this")();
  }}
}})();
const defines = {{{defines}}};
Object.keys(defines).forEach((key) => {{
  const segments = key.split(".");
  let target = context;
  for (let i = 0; i < segments.length; i++) {{
    const segment = segments[i];
    if (i === segments.length - 1) {{
      target[segment] = defines[key];
    }} else {{
      target = target[segment] || (target[segment] = {{}});
    }}
  }}
}});
"#
    )
}

/// A worker's dev URL (vite:worker's load): fileToUrl of the clean file with
/// `worker_file&type=<type>` injected. The type is `module`, as an import of
/// `./w.js?worker` always is in dev, unless oj's own rewrite of
/// `new Worker(new URL(...), opts)` passed the options' one (`worker_type`).
fn worker_url(urls: &DevAssetUrls, url: &str) -> String {
    let (clean, query) = url.split_once('?').unwrap_or((url, ""));
    let worker_type = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("worker_type="))
        .filter(|t| matches!(*t, "module" | "classic" | "ignore"))
        .unwrap_or("module");
    urls.join(&format!("{clean}?worker_file&type={worker_type}"))
}

/// What vite:worker's transform prepends to a `?worker_file&type=<type>`
/// module: the env module imported (module), loaded with importScripts under
/// the base (classic), or inlined (ignore: its type is unknown); None for
/// another request.
pub(crate) fn worker_file_prelude(url: &str, base: &str, env: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    let mut kvs = query.split('&');
    if !kvs.any(|kv| kv == "worker_file") {
        return None;
    }
    let worker_type = query.split('&').find_map(|kv| kv.strip_prefix("type="))?;
    let env_path = format!("{base}/@vite/env");
    let inject = match worker_type {
        "module" => format!("import {}\n", serde_json::Value::String(env_path)),
        "classic" => format!("importScripts({})\n", serde_json::Value::String(env_path)),
        "ignore" => env.to_string(),
        _ => return None,
    };
    Some(inject + ";\n")
}

fn js_default(value: String) -> String {
    format!("export default {};\n", serde_json::Value::String(value))
}

pub(crate) async fn asset_module(
    file: &Path,
    url: &str,
    kind: &str,
    urls: &DevAssetUrls,
    env: &str,
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
            // `?worker&url`: vite:worker's load exports the worker's URL.
            if has_param(url, "worker") || has_param(url, "sharedworker") {
                return Ok(js_default(worker_url(urls, url)));
            }
            // vite:asset's load exports `encodeURIPath(url)`.
            Ok(js_default(encode_uri_path(&urls.file_url(env, file, url)?)))
        }
        "raw" => {
            let text = tokio::fs::read_to_string(file)
                .await
                .map_err(|e| format!("read {}: {e}", file.display()))?;
            Ok(js_default(text))
        }
        "inline" => {
            // A stylesheet's `?inline` goes through `inline_css_module` instead.
            let bytes = tokio::fs::read(file)
                .await
                .map_err(|e| format!("read: {e}"))?;
            Ok(js_default(asset_data_url(file, &bytes)))
        }
        // Vite's worker and wasm plugins name the file with fileToUrl too.
        // vite:worker's dev load: a WorkerWrapper passing `options.name`.
        "worker" | "sharedworker" => {
            let ctor = if kind == "sharedworker" {
                "SharedWorker"
            } else {
                "Worker"
            };
            let url = serde_json::Value::String(worker_url(urls, url));
            Ok(format!(
                "export default function WorkerWrapper(options) {{\n  return new {ctor}(\n    {url},\n    {{\n      type: \"module\",\n      name: options?.name\n    }}\n  );\n}}\n"
            ))
        }
        "init" => {
            let url = serde_json::Value::String(urls.join(clean_url));
            Ok(format!(
                "export default (imports = {{}}) => {{\n  const url = {url};\n  const inst = (r) => r.instance;\n  const fallback = () => fetch(url).then((r) => r.arrayBuffer()).then((b) => WebAssembly.instantiate(b, imports)).then(inst);\n  if (WebAssembly.instantiateStreaming) {{\n    return WebAssembly.instantiateStreaming(fetch(url), imports).then(inst).catch(fallback);\n  }}\n  return fallback();\n}};\n"
            ))
        }
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

/// fileToDevUrl's data URL for `file` requested as `url` under a numeric
/// `assetsInlineLimit`, when it inlines (`?inline`, or a small svg without
/// `?no-inline` or a fragment).
pub fn inline_url_under(file: &Path, url: &str, limit: u64) -> Option<String> {
    DevAssetUrls::new(None, None, Some(limit))
        .inline_url("client", file, url)
        .ok()
        .flatten()
}
