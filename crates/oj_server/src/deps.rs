use super::*;

// Partial bundling (oj-native per-package dep bundling) is opt-in for now.
pub(crate) fn partial_bundle_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("OJ_PARTIAL_BUNDLE").is_ok_and(|v| !v.is_empty() && v != "0"))
}

// The URL a resolved bare dep is served from: partial bundling serves a CJS package
// as one shared `/@oj-pkg` bundle keyed by entry path, everything else per-file.
// Both the interop and specifier-rewrite paths route here so a dep never gets two URLs.
pub(crate) fn dep_serve_url(resolved: &Path, root: &Path) -> String {
    if partial_bundle_enabled()
        && in_node_modules(resolved)
        && is_bundleable_dep_file(resolved)
        // optimizeDeps.exclude: serve this package per-file, never bundled.
        && !pkg_bundle::is_excluded(resolved)
    {
        return pkg_bundle::bundle_url_for(resolved);
    }
    url_of(root, resolved)
}

/// Vite's DEP_VERSION_RE: the request carries a `v=` query, i.e. it was reached
/// through a versioned dep URL and may be cached forever.
pub(crate) fn has_version_query(query: Option<&str>) -> bool {
    query.is_some_and(|q| q.split('&').any(|kv| kv.starts_with("v=")))
}

/// Vite transform middleware: an optimized dep is `max-age=31536000,immutable`
/// (its URL changes with the prebundle hash), everything else `no-cache`.
pub(crate) fn dep_cache_control(versioned: bool) -> &'static str {
    if versioned {
        "max-age=31536000,immutable"
    } else {
        "no-cache"
    }
}

/// Prebundled dep response: strong ETag with 304 on If-None-Match (Vite's send()),
/// immutable cache policy when the URL is versioned.
pub(crate) fn dep_response(headers: &HeaderMap, versioned: bool, bytes: Vec<u8>) -> Response {
    let etag = format!("\"{}\"", &blake3::hash(&bytes).to_hex()[..16]);
    let cache_control = dep_cache_control(versioned).to_string();
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|inm| inm.split(',').any(|t| t.trim() == etag))
    {
        return (
            StatusCode::NOT_MODIFIED,
            [(header::ETAG, etag), (header::CACHE_CONTROL, cache_control)],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "text/javascript".to_string()),
            (header::CACHE_CONTROL, cache_control),
            (header::ETAG, etag),
        ],
        bytes,
    )
        .into_response()
}

pub(crate) const OPTIONAL_PEER_PREFIX: &str = "/@oj-optional-peer/";

/// Vite's optionalPeerDepId: an unresolved bare import from inside a dep whose
/// nearest package.json marks it an optional peer gets a stub that errors only on eval.
pub(crate) fn optional_peer_dep_url(root: &Path, dir: &Path, spec: &str) -> Option<String> {
    if !is_bare_specifier(spec) || spec.is_empty() || is_node_builtin(spec) || spec.contains('\0') {
        return None;
    }
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if dir == root || !in_node_modules(&dir) {
        return None;
    }
    let pkg_name = {
        let mut it = spec.split('/');
        let first = it.next()?;
        if first.starts_with('@') {
            format!("{first}/{}", it.next()?)
        } else {
            first.to_string()
        }
    };
    // findNearestMainPackageData: the closest package.json with a `name`.
    let mut cur: Option<&Path> = Some(dir.as_path());
    while let Some(d) = cur {
        if let Ok(txt) = std::fs::read_to_string(d.join("package.json")) {
            if let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&txt) {
                if let Some(parent) = pkg.get("name").and_then(|n| n.as_str()) {
                    let declared = pkg
                        .get("peerDependencies")
                        .and_then(|p| p.get(&pkg_name))
                        .is_some();
                    let optional = pkg
                        .get("peerDependenciesMeta")
                        .and_then(|m| m.get(&pkg_name))
                        .and_then(|m| m.get("optional"))
                        .and_then(|o| o.as_bool())
                        .unwrap_or(false);
                    if declared && optional {
                        return Some(format!(
                            "{OPTIONAL_PEER_PREFIX}{}",
                            hex_encode(&format!("{spec}\n{parent}"))
                        ));
                    }
                    return None;
                }
            }
        }
        if d.file_name().is_some_and(|n| n == "node_modules") {
            return None;
        }
        cur = d.parent();
    }
    None
}

/// The module `/@oj-optional-peer/<hex>` serves (Vite's optional peer stub):
/// evaluating it throws an error naming both the peer and the parent package.
pub(crate) fn optional_peer_dep_stub(hex: &str) -> Option<String> {
    let decoded = hex_decode(hex)?;
    let (peer, parent) = decoded.split_once('\n')?;
    let peer_js = serde_json::Value::String(peer.to_string());
    let parent_js = serde_json::Value::String(parent.to_string());
    Some(format!(
        "// oj: optional peer dependency {peer_js} of {parent_js} is not installed\n\
         export default {{}};\n\
         export const __cjs_exports = {{}};\n\
         throw new Error(`Could not resolve \"${{{peer_js}}}\" imported by \"${{{parent_js}}}\". Is it installed?`);\n"
    ))
}
