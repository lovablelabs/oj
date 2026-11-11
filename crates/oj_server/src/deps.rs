use super::*;

/// `GET <cacheDir>/deps/<flattenId(dep)>.js` (`/node_modules/.vite/deps/
/// react.js` under the default `cacheDir`), Vite's URL for an optimized dep:
/// a module re-exporting the pre-bundle entry oj's own import rewriting
/// hands the app for that dep, at the URL it hands it (`/@oj-deps/
/// <file>?v=<version>`). The browser keys its module map by the URL it
/// requested, so serving the pre-bundle's bytes here (or redirecting) would
/// evaluate the dep a second time next to the app's copy: two Reacts. The
/// re-export holds no state of its own, so a script that imports Vite's path
/// gets the one instance the page runs on. A stem the optimizer does not
/// know but that decodes to an installed dependency registers it, as Vite's
/// resolve does for a missing import (registerMissingImport), and the request
/// waits for the re-optimization like any dep request; one whose rerun then
/// commits without it (the bundle failed) is Vite's processing error, a 504;
/// any other stem is the 404 Vite's optimizedDeps load gives a file that is
/// not in its cache dir. Vite's `?v=` is its own browserHash and never oj's:
/// it is ignored rather than 504ed as outdated, and the response is
/// `no-cache` since the URL it re-exports moves with every rerun.
pub(crate) async fn serve_vite_dep_url(
    state: &Arc<ServerState>,
    headers: &HeaderMap,
    path: &str,
) -> Response {
    let file = path
        .strip_prefix(state.vite_deps_prefix.as_str())
        .unwrap_or(path);
    if file.contains('/') || file.contains("..") {
        return (StatusCode::FORBIDDEN, "oj: bad optimized dep path").into_response();
    }
    state.optimized.ready().await;
    let Some(dep) = vite_dep_id(state, path) else {
        return (
            StatusCode::NOT_FOUND,
            format!("oj: no optimized dep {file}"),
        )
            .into_response();
    };
    let registered = state.optimized.meta_now(&dep).or_else(|| {
        let resolved = state.resolver.resolve(&state.root, &dep).ok()?;
        register_missing_import(state, &dep, &resolved)
    });
    let Some(meta) = registered else {
        return (
            StatusCode::NOT_FOUND,
            format!("oj: no optimized dep {file}"),
        )
            .into_response();
    };
    // A registered dep's request holds until its rerun commits; the commit
    // may have moved the URL, so the live entry is read after the wait. A
    // commit without the dep (its bundle failed, or it could not be
    // pre-bundled) is Vite's processing error: a 504, never a re-export of
    // a URL nothing serves. Its importers recompile onto the per-file path
    // after the reload the failure sent, and the dep registers no more.
    state.optimized.await_dep(&meta.file, None).await;
    let Some(meta) = state.optimized.meta_now(&dep) else {
        return (
            StatusCode::GATEWAY_TIMEOUT,
            format!("oj: optimizing {dep} failed"),
        )
            .into_response();
    };
    let mut code = format!("export * from \"{}\";\n", meta.url);
    if pre_bundle_has_default(state, &meta) {
        code.push_str(&format!("export {{ default }} from \"{}\";\n", meta.url));
    }
    dep_response(headers, false, code.into_bytes())
}

/// Whether the pre-bundle entry has a default export to re-export: a
/// CommonJS dep's always does (its bundle exports `module.exports` as the
/// default, and the named-export facade re-exports it); an ES module dep's
/// entry is read, as `export *` passes over the default and a re-export of
/// one the entry lacks would fail to link.
fn pre_bundle_has_default(state: &ServerState, meta: &optimize::DepMeta) -> bool {
    if meta.needs_interop || meta.file.ends_with("__oj_named.mjs") {
        return true;
    }
    let path = state.optimized.dir().join(&meta.file);
    std::fs::read_to_string(&path)
        .map(|code| {
            oj_compiler::exports(&code, &path)
                .iter()
                .any(|e| e == "default")
        })
        .unwrap_or(false)
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
