// SPDX-License-Identifier: MIT

//! `GET /@oj/tanstack/routes.json`: the routes of a TanStack Start app in
//! dev, for a host UI that lists them (a preview's route picker).
//!
//! They are read from the generated route tree (`routeTree.gen.ts`), which
//! the router generator rewrites as route files come and go. The part read is
//! the one every form of that file has, typed or not (`disableTypes`, a `.js`
//! tree), in either quote style:
//!
//! ```js
//! import { Route as PostsPostIdRouteImport } from './routes/posts/$postId'
//! const PostsPostIdRoute = PostsPostIdRouteImport.update({
//!   id: '/$postId',
//!   path: '/$postId',
//!   getParentRoute: () => PostsRoute,
//! } as any)
//! ```
//!
//! `id` and `path` are relative to the parent, so a route's full id and full
//! path are its ancestors' joined with its own, which gives the strings the
//! file's `FileRoutesById` and `FileRoutesByFullPath` interfaces list (the
//! tests hold the two against each other on a tree the generator wrote). A
//! pathless layout route (`_layout.tsx`) has no `path` there and no entry in
//! `FileRoutesByFullPath`: it is in no route's way to a URL of its own, so it
//! is not listed, and neither is the root, which has no `update` call.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// The URL, past the dev `base`.
pub(crate) const PATH: &str = "/@oj/tanstack/routes.json";

/// One route a picker can navigate to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteEntry {
    /// The full path as the router matches it (`/posts/$postId`).
    pub path: String,
    /// The route id (`/_layout/dash` where the path is `/dash`).
    pub id: String,
    /// The route's module relative to the app root, as the tree imports it:
    /// without an extension unless the generator wrote one (`addExtensions`).
    /// [`with_extensions`] completes it from the disk.
    pub file: Option<String>,
    /// The names of the dynamic segments of `path`, in order; left out of
    /// the JSON when there are none.
    pub params: Vec<String>,
}

#[derive(Debug, PartialEq)]
enum Tok<'a> {
    Ident(&'a str),
    Str(String),
    Punct(char),
}

/// The file as identifiers, string literals and punctuation, comments
/// dropped: enough to find the generator's statements however they are
/// wrapped or quoted.
fn tokens(text: &str) -> Vec<Tok<'_>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
        } else if matches!(b, b'\'' | b'"' | b'`') {
            // Quotes and backslashes are ASCII, so the slices between them
            // are whole characters.
            let mut value = String::new();
            let mut start = i + 1;
            i += 1;
            while i < bytes.len() && bytes[i] != b {
                if bytes[i] == b'\\' {
                    value.push_str(&text[start..i]);
                    start = i + 1;
                    // The escaped character, whatever its width.
                    i += 1 + text[i + 1..].chars().next().map_or(0, char::len_utf8);
                } else {
                    i += 1;
                }
            }
            let end = i.min(bytes.len());
            value.push_str(text.get(start..end).unwrap_or(""));
            out.push(Tok::Str(value));
            i = end + 1;
        } else if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
            {
                i += 1;
            }
            out.push(Tok::Ident(&text[start..i]));
        } else {
            let c = text[i..].chars().next().unwrap_or(' ');
            out.push(Tok::Punct(c));
            i += c.len_utf8().max(1);
        }
    }
    out
}

/// One `X.update({ ... })` of the tree.
#[derive(Debug, Default)]
struct Node<'a> {
    /// The route object updated (`PostsPostIdRouteImport`).
    import: &'a str,
    id: Option<String>,
    path: Option<String>,
    parent: Option<&'a str>,
    /// The module of `.lazy(() => import('...'))`.
    lazy: Option<String>,
}

fn ident<'a>(toks: &[Tok<'a>], i: usize) -> Option<&'a str> {
    match toks.get(i) {
        Some(Tok::Ident(name)) => Some(name),
        _ => None,
    }
}

fn is_ident(toks: &[Tok<'_>], i: usize, name: &str) -> bool {
    ident(toks, i) == Some(name)
}

fn is_punct(toks: &[Tok<'_>], i: usize, c: char) -> bool {
    toks.get(i) == Some(&Tok::Punct(c))
}

fn string(toks: &[Tok<'_>], i: usize) -> Option<String> {
    match toks.get(i) {
        Some(Tok::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

/// `Route as X ... from 'spec'`: the module each route object comes from.
fn imports<'a>(toks: &[Tok<'a>]) -> HashMap<&'a str, String> {
    let mut out = HashMap::new();
    for i in 0..toks.len() {
        if !(is_ident(toks, i, "Route") && is_ident(toks, i + 1, "as")) {
            continue;
        }
        let Some(name) = ident(toks, i + 2) else {
            continue;
        };
        let mut j = i + 3;
        while is_punct(toks, j, ',') || is_punct(toks, j, '}') {
            j += 1;
        }
        if is_ident(toks, j, "from") {
            if let Some(spec) = string(toks, j + 1) {
                out.insert(name, spec);
            }
        }
    }
    out
}

/// Every `Var = Import.update({ ... })`, by `Var`.
fn nodes<'a>(toks: &[Tok<'a>]) -> Vec<(&'a str, Node<'a>)> {
    let mut out = Vec::new();
    for i in 0..toks.len() {
        let (Some(var), Some(import)) = (ident(toks, i), ident(toks, i + 2)) else {
            continue;
        };
        if !(is_punct(toks, i + 1, '=')
            && is_punct(toks, i + 3, '.')
            && is_ident(toks, i + 4, "update")
            && is_punct(toks, i + 5, '(')
            && is_punct(toks, i + 6, '{'))
        {
            continue;
        }
        let mut node = Node {
            import,
            ..Node::default()
        };
        let mut depth = 1usize;
        let mut j = i + 7;
        while j < toks.len() && depth > 0 {
            match &toks[j] {
                Tok::Punct('{' | '(' | '[') => depth += 1,
                Tok::Punct('}' | ')' | ']') => depth -= 1,
                Tok::Ident(key) if depth == 1 && is_punct(toks, j + 1, ':') => match *key {
                    "id" => node.id = string(toks, j + 2).or(node.id),
                    "path" => node.path = string(toks, j + 2).or(node.path),
                    // `() => Parent`
                    "getParentRoute" => {
                        let arrow = (j + 2..(j + 8).min(toks.len()))
                            .find(|&k| is_punct(toks, k, '=') && is_punct(toks, k + 1, '>'));
                        node.parent = arrow.and_then(|k| ident(toks, k + 2)).or(node.parent);
                    }
                    _ => {}
                },
                _ => {}
            }
            j += 1;
        }
        // `} as any).lazy(() => import('./routes/x.lazy').then(...))`
        if is_ident(toks, j, "as") {
            j += 2;
        }
        if is_punct(toks, j, ')') && is_punct(toks, j + 1, '.') && is_ident(toks, j + 2, "lazy") {
            node.lazy = (j + 3..(j + 12).min(toks.len()))
                .find(|&k| is_ident(toks, k, "import") && is_punct(toks, k + 1, '('))
                .and_then(|k| string(toks, k + 2));
        }
        out.push((var, node));
    }
    out
}

/// `a` then `b` as one path: a single leading slash, no doubled ones, a
/// trailing one kept (an index route under `/posts` is `/posts/`).
fn join(a: &str, b: &str) -> String {
    let mut out = String::with_capacity(a.len() + b.len() + 2);
    for c in "/"
        .chars()
        .chain(a.chars())
        .chain("/".chars())
        .chain(b.chars())
    {
        if !(c == '/' && out.ends_with('/')) {
            out.push(c);
        }
    }
    if b.is_empty() && out.len() > 1 {
        out.pop();
    }
    out
}

/// The full id and the full path of `var`'s route: its ancestors' joined
/// with its own. A parent the tree has no `update` for is the root.
fn full(var: &str, by_var: &HashMap<&str, &Node<'_>>) -> Option<(String, String)> {
    let mut chain = Vec::new();
    let mut cur = Some(var);
    while let Some(node) = cur.and_then(|name| by_var.get(name)) {
        // A tree deeper than this is a cycle in a file nobody generated.
        if chain.len() >= 64 {
            return None;
        }
        chain.push(*node);
        cur = node.parent;
    }
    let (mut id, mut path) = (String::new(), String::new());
    for node in chain.iter().rev() {
        id = join(&id, node.id.as_deref()?);
        path = join(&path, node.path.as_deref().unwrap_or(""));
    }
    Some((id, path))
}

/// The dynamic segments of `path` by name: `$postId`, the optional
/// `{-$locale}`, a prefixed or suffixed `file-{$name}.txt`, and the splat
/// (`$`, `{$}`) under the router's own name for it, `_splat`.
fn params(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    for segment in path.split('/') {
        let name = if let Some(name) = segment.strip_prefix('$') {
            name
        } else if let (Some(open), Some(close)) = (segment.find('{'), segment.rfind('}')) {
            let inner = segment.get(open + 1..close).unwrap_or("");
            match inner.strip_prefix('-').unwrap_or(inner).strip_prefix('$') {
                Some(name) => name,
                None => continue,
            }
        } else {
            continue;
        };
        out.push(if name.is_empty() { "_splat" } else { name }.to_string());
    }
    out
}

/// `spec`, an import of the tree at `tree`, as a path relative to `root`
/// with forward slashes. None for a bare specifier or a file outside the
/// root.
fn file_of(spec: &str, tree: &Path, root: &Path) -> Option<String> {
    let path = if spec.starts_with('.') {
        tree.parent()?.join(spec)
    } else if Path::new(spec).is_absolute() {
        PathBuf::from(spec)
    } else {
        return None;
    };
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !clean.pop() {
                    return None;
                }
            }
            other => clean.push(other),
        }
    }
    let parts: Vec<_> = clean
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The routes the generated tree `text` (the file at `tree`, of the app at
/// `root`) declares with a path of their own, sorted by path then id. Text
/// that is not a generated tree has none.
pub(crate) fn routes(text: &str, tree: &Path, root: &Path) -> Vec<RouteEntry> {
    let toks = tokens(text);
    let imports = imports(&toks);
    let nodes = nodes(&toks);
    let by_var: HashMap<&str, &Node<'_>> = nodes.iter().map(|(var, node)| (*var, node)).collect();
    let mut out = Vec::new();
    for (var, node) in &nodes {
        // No path of its own: a pathless layout.
        if node.path.as_deref().is_none_or(str::is_empty) {
            continue;
        }
        let Some((id, path)) = full(var, &by_var) else {
            continue;
        };
        let file = imports
            .get(node.import)
            .or(node.lazy.as_ref())
            .and_then(|spec| file_of(spec, tree, root));
        out.push(RouteEntry {
            params: params(&path),
            path,
            id,
            file,
        });
    }
    out.sort_by(|a, b| (&a.path, &a.id).cmp(&(&b.path, &b.id)));
    out.dedup();
    out
}

/// Each route's `file` with the extension its file has on disk, where the
/// tree imported it without one. A file that is not there keeps the import's
/// spelling.
pub(crate) fn with_extensions(routes: &mut [RouteEntry], root: &Path) {
    for route in routes {
        let Some(file) = &route.file else { continue };
        if root.join(file).is_file() {
            continue;
        }
        // Appended, not replaced: `a.b.$c` is a whole name, not `a` + `.b.$c`.
        let found = ["tsx", "ts", "jsx", "js", "mts", "mjs", "cts", "cjs", "vue"]
            .iter()
            .map(|ext| format!("{file}.{ext}"))
            .find(|candidate| root.join(candidate).is_file());
        if let Some(found) = found {
            route.file = Some(found);
        }
    }
}

/// The response body: `{"routes":[{"path","id","file"?,"params"?}]}`, the
/// keys in that order.
pub(crate) fn json(routes: &[RouteEntry]) -> String {
    let quoted = |s: &str| serde_json::Value::from(s).to_string();
    let entries: Vec<String> = routes
        .iter()
        .map(|route| {
            let mut entry = format!(
                r#"{{"path":{},"id":{}"#,
                quoted(&route.path),
                quoted(&route.id)
            );
            if let Some(file) = &route.file {
                entry.push_str(&format!(r#","file":{}"#, quoted(file)));
            }
            if !route.params.is_empty() {
                let names: Vec<String> = route.params.iter().map(|p| quoted(p)).collect();
                entry.push_str(&format!(r#","params":[{}]"#, names.join(",")));
            }
            entry.push('}');
            entry
        })
        .collect();
    format!(r#"{{"routes":[{}]}}"#, entries.join(","))
}

/// The body for the app at `root` whose generated tree is `tree`. A tree
/// that is not there (the generator has not run yet) or cannot be read is an
/// app with no routes to list.
pub(crate) fn body(tree: &Path, root: &Path) -> String {
    let mut found = match std::fs::read(tree) {
        Ok(bytes) => routes(&String::from_utf8_lossy(&bytes), tree, root),
        Err(_) => Vec::new(),
    };
    with_extensions(&mut found, root);
    json(&found)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree router-generator 1.167 wrote for an app with every kind of
    /// route file: index, nested, dynamic, splat, optional, prefixed param,
    /// pathless layouts (at the root and nested), route groups, a lazy-only
    /// route, an escaped dot, a non-nested (`posts_.`) route.
    const GENERATED: &str = include_str!("testdata/route-tree.gen.ts.txt");

    fn parse(text: &str) -> Vec<RouteEntry> {
        routes(
            text,
            Path::new("/app/src/routeTree.gen.ts"),
            Path::new("/app"),
        )
    }

    fn entry(path: &str, id: &str, file: Option<&str>, params: &[&str]) -> RouteEntry {
        RouteEntry {
            path: path.into(),
            id: id.into(),
            file: file.map(Into::into),
            params: params.iter().map(|p| p.to_string()).collect(),
        }
    }

    fn update(var: &str, id: &str, path: Option<&str>, parent: &str) -> String {
        let path = path.map_or(String::new(), |p| format!("  path: '{p}',\n"));
        format!(
            "const {var} = {var}Import.update({{\n  id: '{id}',\n{path}  getParentRoute: () => {parent},\n}} as any)\n"
        )
    }

    /// The keys of `export interface <name> { '<key>': ... }` in the tree.
    fn interface_keys(text: &str, name: &str) -> Vec<String> {
        let start = text.find(&format!("export interface {name} {{")).unwrap();
        let body = &text[start..start + text[start..].find("\n}").unwrap()];
        let mut keys: Vec<String> = body
            .lines()
            .filter_map(|line| line.trim().strip_prefix('\''))
            .filter_map(|rest| rest.split_once('\''))
            .map(|(key, _)| key.to_string())
            .collect();
        keys.sort();
        keys
    }

    #[test]
    fn a_generated_tree_lists_what_its_full_path_interface_lists() {
        let found = parse(GENERATED);
        let paths: Vec<String> = found.iter().map(|r| r.path.clone()).collect();
        assert_eq!(paths, interface_keys(GENERATED, "FileRoutesByFullPath"));
        // Every id is one the tree declares; the two it declares and are not
        // listed are the pathless layouts.
        let mut ids: Vec<String> = found.iter().map(|r| r.id.clone()).collect();
        ids.extend(["/_layout".to_string(), "/posts/_inner".to_string()]);
        ids.sort();
        assert_eq!(ids, interface_keys(GENERATED, "FileRoutesById"));
    }

    #[test]
    fn a_generated_tree_entry_by_entry() {
        let found = parse(GENERATED);
        let of = |id: &str| {
            found
                .iter()
                .find(|r| r.id == id)
                .unwrap_or_else(|| panic!("no route {id}: {found:#?}"))
                .clone()
        };
        for expected in [
            entry("/", "/", Some("src/routes/index"), &[]),
            entry("/about", "/about", Some("src/routes/about"), &[]),
            // A layout route with a path, and the index route under it.
            entry("/posts", "/posts", Some("src/routes/posts"), &[]),
            entry("/posts/", "/posts/", Some("src/routes/posts/index"), &[]),
            entry(
                "/posts/$postId",
                "/posts/$postId",
                Some("src/routes/posts/$postId"),
                &["postId"],
            ),
            entry(
                "/files/$",
                "/files/$",
                Some("src/routes/files/$"),
                &["_splat"],
            ),
            entry(
                "/{-$locale}/home",
                "/{-$locale}/home",
                Some("src/routes/{-$locale}/home"),
                &["locale"],
            ),
            entry(
                "/files/pre{$id}suf",
                "/files/pre{$id}suf",
                Some("src/routes/files/pre{$id}suf"),
                &["id"],
            ),
            // Under pathless layouts, at the root and nested.
            entry(
                "/dash",
                "/_layout/dash",
                Some("src/routes/_layout/dash"),
                &[],
            ),
            entry(
                "/posts/deep",
                "/posts/_inner/deep",
                Some("src/routes/posts/_inner/deep"),
                &[],
            ),
            // Route groups.
            entry(
                "/login",
                "/(group)/login",
                Some("src/routes/(group)/login"),
                &[],
            ),
            entry(
                "/posts/in",
                "/posts/(grp)/in",
                Some("src/routes/posts/(grp)/in"),
                &[],
            ),
            // Dot-form names, a non-nested one, an escaped dot.
            entry("/a/b/$c", "/a/b/$c", Some("src/routes/a.b.$c"), &["c"]),
            entry(
                "/users/$id/edit",
                "/users_/$id/edit",
                Some("src/routes/users_.$id.edit"),
                &["id"],
            ),
            entry(
                "/script.js",
                "/script.js",
                Some("src/routes/script[.]js"),
                &[],
            ),
            // Only a `.lazy` file: the tree makes the route and names the
            // file in its `.lazy(() => import(...))`.
            entry(
                "/lazyonly",
                "/lazyonly",
                Some("src/routes/lazyonly.lazy"),
                &[],
            ),
        ] {
            assert_eq!(of(&expected.id), expected);
        }
        assert!(found
            .iter()
            .all(|r| r.id != "/_layout" && r.id != "__root__"));
        let mut sorted = found.clone();
        sorted.sort_by(|a, b| (&a.path, &a.id).cmp(&(&b.path, &b.id)));
        assert_eq!(found, sorted);
    }

    #[test]
    fn an_untyped_tree_in_double_quotes_with_extensions_reads_the_same() {
        let text = r#"
import { createFileRoute } from "@tanstack/react-router";
import { Route as rootRouteImport } from "./routes/__root.tsx";
import { Route as PostsRouteImport } from "./routes/posts.tsx";
import { Route as PostsPostIdRouteImport } from "./routes/posts/$postId.tsx";

const LazyonlyLazyRouteImport = createFileRoute("/lazyonly")();

const PostsRoute = PostsRouteImport.update({
  id: "/posts",
  path: "/posts",
  getParentRoute: () => rootRouteImport,
});
const LazyonlyLazyRoute = LazyonlyLazyRouteImport.update({
  id: "/lazyonly",
  path: "/lazyonly",
  getParentRoute: () => rootRouteImport,
}).lazy(() => import("./routes/lazyonly.lazy.tsx").then((d) => d.Route));
const PostsPostIdRoute =
  PostsPostIdRouteImport.update({
    id: "/$postId",
    path: "/$postId",
    getParentRoute: () => PostsRoute,
  });
export const routeTree = rootRouteImport._addFileChildren(rootRouteChildren);
"#;
        assert_eq!(
            parse(text),
            [
                entry(
                    "/lazyonly",
                    "/lazyonly",
                    Some("src/routes/lazyonly.lazy.tsx"),
                    &[]
                ),
                entry("/posts", "/posts", Some("src/routes/posts.tsx"), &[]),
                entry(
                    "/posts/$postId",
                    "/posts/$postId",
                    Some("src/routes/posts/$postId.tsx"),
                    &["postId"]
                ),
            ]
        );
    }

    #[test]
    fn index_nested_and_dynamic_routes() {
        let text = [
            update("IndexRoute", "/", Some("/"), "rootRouteImport"),
            update("ShopRoute", "/shop", Some("/shop"), "rootRouteImport"),
            update("ShopIndexRoute", "/", Some("/"), "ShopRoute"),
            update("ShopItemRoute", "/$itemId", Some("/$itemId"), "ShopRoute"),
            update(
                "ShopItemReviewRoute",
                "/reviews/$reviewId",
                Some("/reviews/$reviewId"),
                "ShopItemRoute",
            ),
        ]
        .concat();
        assert_eq!(
            parse(&text),
            [
                entry("/", "/", None, &[]),
                entry("/shop", "/shop", None, &[]),
                entry("/shop/", "/shop/", None, &[]),
                entry("/shop/$itemId", "/shop/$itemId", None, &["itemId"]),
                entry(
                    "/shop/$itemId/reviews/$reviewId",
                    "/shop/$itemId/reviews/$reviewId",
                    None,
                    &["itemId", "reviewId"]
                ),
            ]
        );
    }

    #[test]
    fn splat_and_optional_params_are_named() {
        let text = [
            update(
                "DocsSplatRoute",
                "/docs/$",
                Some("/docs/$"),
                "rootRouteImport",
            ),
            update(
                "LocaleAboutRoute",
                "/{-$locale}/about",
                Some("/{-$locale}/about"),
                "rootRouteImport",
            ),
            update(
                "BracedSplatRoute",
                "/raw/{$}",
                Some("/raw/{$}"),
                "rootRouteImport",
            ),
            update(
                "PlainBracesRoute",
                "/x/{y}",
                Some("/x/{y}"),
                "rootRouteImport",
            ),
        ]
        .concat();
        let found = parse(&text);
        let params_of = |path: &str| {
            found
                .iter()
                .find(|r| r.path == path)
                .unwrap()
                .params
                .clone()
        };
        assert_eq!(params_of("/docs/$"), ["_splat"]);
        assert_eq!(params_of("/{-$locale}/about"), ["locale"]);
        assert_eq!(params_of("/raw/{$}"), ["_splat"]);
        assert_eq!(params_of("/x/{y}"), Vec::<String>::new());
    }

    #[test]
    fn a_pathless_layout_is_not_listed_and_its_children_keep_its_id() {
        let text = [
            update("AuthRoute", "/_auth", None, "rootRouteImport"),
            update("AuthIndexRoute", "/", Some("/"), "AuthRoute"),
            update(
                "AuthSettingsRoute",
                "/settings",
                Some("/settings"),
                "AuthRoute",
            ),
        ]
        .concat();
        assert_eq!(
            parse(&text),
            [
                entry("/", "/_auth/", None, &[]),
                entry("/settings", "/_auth/settings", None, &[]),
            ]
        );
    }

    #[test]
    fn a_route_group_is_in_the_id_and_not_in_the_path() {
        let text = update(
            "marketingPricingRoute",
            "/(marketing)/pricing",
            Some("/pricing"),
            "rootRouteImport",
        );
        assert_eq!(
            parse(&text),
            [entry("/pricing", "/(marketing)/pricing", None, &[])]
        );
    }

    #[test]
    fn a_tree_with_no_routes_lists_none() {
        // What the generator writes for an app whose routes are code-based:
        // the root alone.
        let text = r#"
import { Route as rootRouteImport } from './routes/__root'

export interface FileRoutesByFullPath {}
export interface FileRoutesById {
  __root__: typeof rootRouteImport
}
export interface FileRouteTypes {
  fullPaths: never
  id: '__root__'
}
const rootRouteChildren: RootRouteChildren = {}
export const routeTree = rootRouteImport
  ._addFileChildren(rootRouteChildren)
  ._addFileTypes<FileRouteTypes>()
"#;
        assert_eq!(parse(text), []);
        assert_eq!(json(&parse(text)), r#"{"routes":[]}"#);
    }

    #[test]
    fn garbage_and_truncated_input_never_panic() {
        for text in [
            "",
            "\u{0}\u{1}\u{2}",
            "not a route tree at all",
            "const A = B.update({",
            "const A = B.update({ id: '/a', path: '/a",
            "const A = B.update({ id: '/a', path: '/a\\",
            "const A = B.update({ id: , path: }",
            "A = B.update({ id: '/a', path: '/a', getParentRoute: () => A })",
            "import { Route as } from",
            "import { Route as X } from 'ünï/çødé'",
            "/* never closed",
            "'never closed \\",
            "é = ü.update({ id: 'é', path: '\\é' })",
            "}}}})))) .update({{{{",
        ] {
            let _ = parse(text);
        }
        // Every prefix of a real tree, cut anywhere a character starts.
        for (end, _) in GENERATED.char_indices().step_by(7) {
            let _ = parse(&GENERATED[..end]);
        }
        // A route that is its own ancestor is dropped, not followed forever.
        let cycle = [
            update("ARoute", "/a", Some("/a"), "BRoute"),
            update("BRoute", "/b", Some("/b"), "ARoute"),
        ]
        .concat();
        assert_eq!(parse(&cycle), []);
        // A route with no id is not listed; its neighbours are.
        let partial = format!(
            "const A = AImport.update({{ path: '/a' }})\n{}",
            update("BRoute", "/b", Some("/b"), "rootRouteImport")
        );
        assert_eq!(parse(&partial), [entry("/b", "/b", None, &[])]);
    }

    #[test]
    fn a_file_is_relative_to_the_app_root_or_left_out() {
        let tree = Path::new("/app/src/gen/tree.gen.ts");
        let root = Path::new("/app");
        assert_eq!(
            file_of("../routes/a", tree, root).as_deref(),
            Some("src/routes/a")
        );
        assert_eq!(
            file_of("./a.tsx", tree, root).as_deref(),
            Some("src/gen/a.tsx")
        );
        assert_eq!(
            file_of("/app/pages/a", tree, root).as_deref(),
            Some("pages/a")
        );
        // Outside the root, a package, nothing.
        assert_eq!(file_of("../../../lib/a", tree, root), None);
        assert_eq!(file_of("/elsewhere/a", tree, root), None);
        assert_eq!(file_of("some-package/route", tree, root), None);
        assert_eq!(file_of("../..", tree, root), None);
    }

    #[test]
    fn the_body_names_files_by_their_extension_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/routes")).unwrap();
        std::fs::write(root.join("src/routes/index.tsx"), "").unwrap();
        std::fs::write(root.join("src/routes/a.b.$c.ts"), "").unwrap();
        let tree = root.join("src/routeTree.gen.ts");
        // No tree yet: no routes, the same shape.
        assert_eq!(body(&tree, root), r#"{"routes":[]}"#);
        let text = [
            "import { Route as IndexRouteImport } from './routes/index'\n".to_string(),
            "import { Route as ABCRouteImport } from './routes/a.b.$c'\n".to_string(),
            "import { Route as GoneRouteImport } from './routes/gone'\n".to_string(),
            update("IndexRoute", "/", Some("/"), "rootRouteImport"),
            update("ABCRoute", "/a/b/$c", Some("/a/b/$c"), "rootRouteImport"),
            update("GoneRoute", "/gone", Some("/gone"), "rootRouteImport"),
        ]
        .concat();
        std::fs::write(&tree, text).unwrap();
        assert_eq!(
            body(&tree, root),
            concat!(
                r#"{"routes":["#,
                r#"{"path":"/","id":"/","file":"src/routes/index.tsx"},"#,
                r#"{"path":"/a/b/$c","id":"/a/b/$c","file":"src/routes/a.b.$c.ts","params":["c"]},"#,
                r#"{"path":"/gone","id":"/gone","file":"src/routes/gone"}"#,
                r#"]}"#
            )
        );
    }
}
