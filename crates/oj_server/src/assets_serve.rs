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
    for kind in ["url", "raw", "worker", "sharedworker", "inline", "init"] {
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

pub(crate) async fn asset_module(file: &Path, url: &str, kind: &str) -> Result<String, String> {
    let clean_url = url.split('?').next().unwrap_or(url);
    match kind {
        "url" => {
            // A stylesheet's URL must serve its COMPILED css (Sass, PostCSS, ...)
            // as text; `?direct` is the plain-CSS request, as in Vite.
            let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
            if is_style_ext(ext) {
                return Ok(format!("export default {:?};\n", format!("{clean_url}?direct")));
            }
            Ok(format!("export default {clean_url:?};\n"))
        }
        "raw" => {
            let text = tokio::fs::read_to_string(file)
                .await
                .map_err(|e| format!("read {}: {e}", file.display()))?;
            Ok(format!("export default {};\n", serde_json::Value::String(text)))
        }
        "inline" => {
            // A stylesheet's `?inline` goes through `inline_css_module` instead.
            let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
            let bytes = tokio::fs::read(file).await.map_err(|e| format!("read: {e}"))?;
            let mime = content_type(ext).split(';').next().unwrap_or("application/octet-stream");
            let data_uri = format!("data:{mime};base64,{}", base64_encode(&bytes));
            Ok(format!("export default {data_uri:?};\n"))
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
    let (_, module) = Box::pin(ensure_module(state, file, clean)).await?;
    Ok(format!(
        "export default {};\n",
        serde_json::to_string(&module.code).unwrap()
    ))
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
