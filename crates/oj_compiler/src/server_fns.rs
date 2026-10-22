// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! TanStack Start's `createServerFn` rewrites, one implementation for every
//! side so they agree on which calls are server functions and on their ids:
//! the server provider (the dev SSR module host and the server bundle) and the
//! client RPC stub (the client bundles).
//!
//! Which calls count is decided textually: `const NAME = createServerFn(...)`
//! followed by a method chain up to the `.handler(` of the SAME expression,
//! with strings, template literals, comments and regex literals skipped, so a
//! `)` in a comment (`// 1) read the key`) cannot end the chain early. A
//! factory split across statements (`const f = createServerFn(); export const
//! x = f.handler(...)`) is left alone: TanStack's compiler resolves that shape
//! through bindings, which a textual pass cannot do safely.

use oxc_allocator::Allocator;
use oxc_ast::ast::{Argument, CallExpression, Expression, Program};
use oxc_ast_visit::{walk, Visit};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};

/// A `const NAME = createServerFn(...)...handler(` declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerFn {
    pub name: String,
    /// Byte offset of the declaration (`export const` / `const`).
    pub decl_start: usize,
    /// Byte offset just past the handler call's `(`.
    pub args_at: usize,
}

/// The id a server function is registered and called under:
/// `base64url("<app-relative path>#<name>")`, Node's `Buffer` encoding.
pub fn server_fn_id(rel: &str, name: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{rel}#{name}").as_bytes())
}

/// Every server function declared in `code`, in source order.
pub fn find_server_fns(code: &str) -> Vec<ServerFn> {
    if !code.contains("createServerFn") {
        return Vec::new();
    }
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"(?:^|[\n;])[ \t]*((?:export\s+)?const\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*createServerFn)\b",
        )
        .expect("server-fn declaration regex")
    });
    re.captures_iter(code)
        .filter_map(|caps| {
            let decl = caps.get(1)?;
            let args_at = scan_chain_to_handler(code, decl.end())?;
            Some(ServerFn {
                name: caps[2].to_string(),
                decl_start: decl.start(),
                args_at,
            })
        })
        .collect()
}

/// The server provider: every server function grows a sibling
/// `NAME_createServerFn_handler = createServerRpc(meta, ...)` export, passed as
/// the handler's first argument. Adds no lines, so stack traces keep their
/// positions. `None` when the module declares no server function.
pub fn rewrite_server(code: &str, rel: &str) -> Option<String> {
    let mut edits: Vec<(usize, String)> = Vec::new();
    for ServerFn {
        name,
        decl_start,
        args_at,
    } in find_server_fns(code)
    {
        let id = server_fn_id(rel, &name);
        let meta = format!("{{ id: {id:?}, name: {name:?}, filename: {rel:?} }}");
        edits.push((
            decl_start,
            format!(
                "export const {name}_createServerFn_handler = createServerRpc({meta}, (opts) => {name}.__executeServer(opts)); "
            ),
        ));
        edits.push((args_at, format!("{name}_createServerFn_handler, ")));
    }
    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|(at, _)| *at);
    let mut out =
        String::from("import { createServerRpc } from \"@tanstack/react-start/server-rpc\"; ");
    let mut last = 0;
    for (at, text) in edits {
        out.push_str(&code[last..at]);
        out.push_str(&text);
        last = at;
    }
    out.push_str(&code[last..]);
    Some(out)
}

/// A client rewrite that could not be applied: a server function the AST walk
/// could not match would ship its handler to the browser, so this is fatal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientRewriteError(pub String);

impl std::fmt::Display for ClientRewriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ClientRewriteError {}

/// The client caller, edited on the module's AST the way TanStack's
/// `handleCreateServerFn` edits its Babel AST: the handler's first argument
/// becomes `createClientRpc(id)` and every `.validator()` / `.inputValidator()`
/// link is dropped, so neither the handler nor the validator reaches the
/// browser. A text scan cannot delimit a handler body (`return /\)/`, a `(` in
/// JSX text), hence the parse. Line counts are kept. `rel` is the app-relative
/// path, which also picks the parser's language. `Ok(None)` when the module
/// declares no server function.
pub fn rewrite_client(code: &str, rel: &str) -> Result<Option<String>, ClientRewriteError> {
    let fns = find_server_fns(code);
    if fns.is_empty() {
        return Ok(None);
    }
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(rel).unwrap_or_else(|_| SourceType::tsx());
    let parsed = Parser::new(&allocator, code, source_type).parse();
    if parsed.fatal_error {
        return Err(ClientRewriteError(format!(
            "oj: could not parse {rel} to rewrite its server functions for the client"
        )));
    }
    let mut finder = HandlerCalls::default();
    finder.visit_program(&parsed.program);

    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for f in &fns {
        let call = finder
            .calls
            .iter()
            .find(|c| c.property_end < f.args_at && f.args_at <= c.first_arg_start);
        let Some((arg_start, arg_end)) = call.and_then(|c| c.first_arg) else {
            return Err(ClientRewriteError(format!(
                "oj: could not rewrite server function {} in {rel} for the client",
                f.name
            )));
        };
        let call = call.expect("matched above");
        edits.push((
            arg_start,
            arg_end,
            format!("createClientRpc({:?})", server_fn_id(rel, &f.name)),
        ));
        for &(start, end) in &call.stripped_links {
            edits.push((start, end, String::new()));
        }
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut out = code.to_string();
    for (start, end, text) in edits {
        let lines = code[start..end].matches('\n').count();
        out.replace_range(start..end, &format!("{text}{}", "\n".repeat(lines)));
    }
    // After the directive prologue (or a hashbang), on its line: the rewrite
    // must not shift source lines, nor turn `"use client"` into a statement.
    let at = prologue_end(&parsed.program, code);
    let sep = if at > 0 && !code[..at].ends_with('\n') {
        " "
    } else {
        ""
    };
    Ok(Some(format!(
        "{}{sep}import {{ createClientRpc }} from \"@tanstack/react-start/client-rpc\"; {}",
        &out[..at],
        &out[at..]
    )))
}

fn prologue_end(program: &Program, code: &str) -> usize {
    if let Some(last) = program.directives.last() {
        return last.span.end as usize;
    }
    if code.starts_with("#!") {
        return code.find('\n').map_or(code.len(), |i| i + 1);
    }
    0
}

/// A `.handler(...)` call whose method chain roots at `createServerFn(...)`.
struct HandlerCall {
    /// End of the `handler` property name.
    property_end: usize,
    /// Start of the first argument, else the call's end.
    first_arg_start: usize,
    first_arg: Option<(usize, usize)>,
    /// `.validator(...)` / `.inputValidator(...)` links to delete, as the span
    /// from the link's object end to the link's end.
    stripped_links: Vec<(usize, usize)>,
}

#[derive(Default)]
struct HandlerCalls {
    calls: Vec<HandlerCall>,
}

/// The method call `callee.object.name(...)` as (object, property name,
/// property end), when `call` is one.
fn method_call<'b, 'a>(call: &'b CallExpression<'a>) -> Option<(&'b Expression<'a>, &'b str, u32)> {
    match &call.callee {
        Expression::StaticMemberExpression(m) => {
            Some((&m.object, m.property.name.as_str(), m.property.span.end))
        }
        _ => None,
    }
}

impl<'a> Visit<'a> for HandlerCalls {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if let Some((object, "handler", property_end)) = method_call(call) {
            let mut stripped = Vec::new();
            let mut node = object;
            let mut roots_at_create = false;
            while let Expression::CallExpression(link) = node {
                match method_call(link) {
                    Some((inner, name, _)) => {
                        if name == "validator" || name == "inputValidator" {
                            stripped.push((inner.span().end as usize, link.span.end as usize));
                        }
                        node = inner;
                    }
                    None => {
                        roots_at_create = matches!(
                            &link.callee,
                            Expression::Identifier(id) if id.name == "createServerFn"
                        );
                        break;
                    }
                }
            }
            if roots_at_create {
                let first_arg = call.arguments.first().map(|a: &Argument| {
                    let span = a.span();
                    (span.start as usize, span.end as usize)
                });
                self.calls.push(HandlerCall {
                    property_end: property_end as usize,
                    first_arg_start: first_arg.map_or(call.span.end as usize, |a| a.0),
                    first_arg,
                    stripped_links: stripped,
                });
            }
        }
        walk::walk_call_expression(self, call);
    }
}

/// Follows a `createServerFn` method chain from `from` (the byte right after
/// the identifier) to the `.handler(` of the same expression, returning the
/// offset just past its opening paren. The chain grammar is strict (a call's
/// balanced arguments, then `.name` and the next call), so the scan ends at
/// anything else: a `;`, a new statement on the next line, an unexpected
/// token.
fn scan_chain_to_handler(code: &str, from: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut i = from;
    loop {
        i = skip_ws_and_comments(bytes, i)?;
        if bytes[i] != b'(' {
            return None;
        }
        i = skip_balanced_parens(bytes, i)?;
        i = skip_ws_and_comments(bytes, i)?;
        if bytes[i] != b'.' {
            return None;
        }
        i = skip_ws_and_comments(bytes, i + 1)?;
        let name_start = i;
        while i < bytes.len()
            && (bytes[i] == b'$' || bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric())
        {
            i += 1;
        }
        if i == name_start {
            return None;
        }
        let method = &code[name_start..i];
        i = skip_ws_and_comments(bytes, i)?;
        if bytes[i] != b'(' {
            return None;
        }
        if method == "handler" {
            return Some(i + 1);
        }
    }
}

fn skip_ws_and_comments(bytes: &[u8], mut i: usize) -> Option<usize> {
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 >= bytes.len() {
                return None;
            }
            i += 2;
            continue;
        }
        return if i < bytes.len() { Some(i) } else { None };
    }
}

/// From an opening `(` at `open`, the index just past its matching `)`. Every
/// bracket kind is tracked; strings, template literals (with nested `${}`
/// code), comments and regex literals are skipped. `None` on EOF or a
/// mismatched bracket.
fn skip_balanced_parens(bytes: &[u8], open: usize) -> Option<usize> {
    debug_assert_eq!(bytes[open], b'(');
    // b'T' marks a template-literal `${` substitution, b'`' template text.
    let mut stack: Vec<u8> = vec![b'('];
    // The previous significant code byte, for the regex-vs-division call.
    let mut prev: Option<u8> = Some(b'(');
    let mut i = open + 1;
    while i < bytes.len() {
        if stack.last() == Some(&b'`') {
            match bytes[i] {
                b'\\' => i += 2,
                b'`' => {
                    stack.pop();
                    // A closed template is a value: a `/` after it divides.
                    prev = Some(b'"');
                    i += 1;
                }
                b'$' if bytes.get(i + 1) == Some(&b'{') => {
                    stack.push(b'T');
                    prev = Some(b'{');
                    i += 2;
                }
                _ => i += 1,
            }
            continue;
        }
        let c = bytes[i];
        match c {
            b'\'' | b'"' => {
                i = skip_string(bytes, i)?;
                prev = Some(b'"');
            }
            b'`' => {
                stack.push(b'`');
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                if i + 1 >= bytes.len() {
                    return None;
                }
                i += 2;
            }
            b'/' if regex_can_follow(prev) => {
                i = skip_regex_literal(bytes, i)?;
                prev = Some(b'/');
            }
            b'(' | b'[' | b'{' => {
                stack.push(c);
                prev = Some(c);
                i += 1;
            }
            b')' | b']' => {
                let want = if c == b')' { b'(' } else { b'[' };
                if stack.pop() != Some(want) {
                    return None;
                }
                if stack.is_empty() {
                    return Some(i + 1);
                }
                prev = Some(c);
                i += 1;
            }
            b'}' => {
                match stack.pop() {
                    Some(b'{') | Some(b'T') => {}
                    _ => return None,
                }
                prev = Some(c);
                i += 1;
            }
            _ => {
                if !c.is_ascii_whitespace() {
                    prev = Some(c);
                }
                i += 1;
            }
        }
    }
    None
}

/// Whether a `/` after this significant byte starts a regex literal rather
/// than a division: after an operator, an opener or a separator it can only
/// be a regex (`.validator((s) => /x\(/.test(s))`), after a value it is a
/// division. Keyword-preceded regexes (`return /x/`) read as division here;
/// inside call arguments, where the scan runs, a bare `return` is rare, and
/// the client rewrite's AST pass covers handler bodies.
fn regex_can_follow(prev: Option<u8>) -> bool {
    match prev {
        None => true,
        Some(c) => matches!(
            c,
            b'(' | b'['
                | b'{'
                | b','
                | b';'
                | b':'
                | b'!'
                | b'&'
                | b'|'
                | b'?'
                | b'='
                | b'+'
                | b'-'
                | b'*'
                | b'%'
                | b'<'
                | b'>'
                | b'~'
                | b'^'
        ),
    }
}

fn skip_string(bytes: &[u8], start: usize) -> Option<usize> {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            c if c == quote => return Some(i + 1),
            // Unterminated: fail soft at the line end.
            b'\n' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

fn skip_regex_literal(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    let mut in_class = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'[' => {
                in_class = true;
                i += 1;
            }
            b']' if in_class => {
                in_class = false;
                i += 1;
            }
            b'/' if !in_class => return Some(i + 1),
            // Not a regex after all.
            b'\n' => return None,
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT_IMPORT: &str =
        "import { createClientRpc } from \"@tanstack/react-start/client-rpc\"; ";

    fn client(code: &str, rel: &str) -> String {
        rewrite_client(code, rel)
            .expect("rewrites")
            .expect("has a server fn")
    }

    fn rpc(rel: &str, name: &str) -> String {
        format!("createClientRpc({:?})", server_fn_id(rel, name))
    }

    /// The client output must still parse as a module.
    fn parses(out: &str, rel: &str) {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, out, SourceType::from_path(rel).unwrap()).parse();
        assert!(
            parsed.diagnostics.is_empty(),
            "{:?}\n{out}",
            parsed.diagnostics
        );
    }

    fn lines(s: &str) -> usize {
        s.split('\n').count()
    }

    #[test]
    fn ids_match_node_buffer_base64url() {
        assert_eq!(server_fn_id("src/a.ts", "fn"), "c3JjL2EudHMjZm4");
    }

    // The shape that blanked a preview: a comment inside the handler with an
    // unbalanced `)`. A paren count ended the handler there and left the rest
    // of the comment and the body in the bundle.
    #[test]
    fn a_paren_inside_a_handler_comment_does_not_end_the_handler() {
        let code = [
            "import { createServerFn } from \"@tanstack/react-start\";",
            "export const saveKey = createServerFn({ method: \"POST\" })",
            "  .handler(async ({ data }) => {",
            "    // 1) chave já gravada localmente para este ambiente",
            "    /* 2) senão, grava :) */",
            "    return data;",
            "  });",
            "export const after = 1;",
            "",
        ]
        .join("\n");
        let rel = "src/qitech.functions.ts";
        let out = client(&code, rel);
        assert!(out.starts_with(CLIENT_IMPORT), "{out}");
        assert!(
            out.contains(&format!(".handler({}\n\n\n\n);", rpc(rel, "saveKey"))),
            "{out}"
        );
        for gone in ["chave", "grava", "return data"] {
            assert!(!out.contains(gone), "{gone} left in:\n{out}");
        }
        assert!(out.contains("export const after = 1;"));
        assert_eq!(lines(&out), lines(&code));
        parses(&out, rel);
    }

    #[test]
    fn brackets_in_strings_templates_and_regexes_stay_inside_the_handler() {
        let code = [
            "export const run = createServerFn()",
            "  .handler(async () => {",
            "    const a = \")((\";",
            "    const b = `x ${f(')')} )`;",
            "    if (/^[)]+\\)/.test(a)) return b;",
            "    return a;",
            "  });",
            "",
        ]
        .join("\n");
        let out = client(&code, "src/run.ts");
        assert!(
            out.contains(&format!(".handler({}", rpc("src/run.ts", "run"))),
            "{out}"
        );
        assert!(!out.contains("return a") && !out.contains(")(("), "{out}");
        parses(&out, "src/run.ts");
    }

    // A `/` right after a template literal divides. Read as a regex start, it
    // lost the chain: the function went unregistered on the server and in
    // the resolver.
    #[test]
    fn a_division_after_a_template_literal_keeps_the_chain() {
        let code = [
            "export const half = createServerFn({ method: `GET` })",
            "  .inputValidator((n: number) => `${n}`.length / 2)",
            "  .handler(async ({ data }) => data);",
            "",
        ]
        .join("\n");
        let found: Vec<String> = find_server_fns(&code).into_iter().map(|f| f.name).collect();
        assert_eq!(found, ["half"]);
        // Right after an opener, the template's own `prev` is the `(`.
        let code = code.replace("`${n}`.length / 2", "(`4`/2) * n");
        let found: Vec<String> = find_server_fns(&code).into_iter().map(|f| f.name).collect();
        assert_eq!(found, ["half"], "a division right after the template");
    }

    // handleCreateServerFn replaces only handlerFnPath, the first argument.
    #[test]
    fn only_the_first_handler_argument_becomes_the_stub() {
        let out = client(
            "export const x = createServerFn().handler(async () => 1, extra);",
            "src/x.ts",
        );
        assert!(
            out.ends_with(&format!(".handler({}, extra);", rpc("src/x.ts", "x"))),
            "{out}"
        );
    }

    #[test]
    fn validator_and_middleware_chains_find_their_handler() {
        let code = [
            "export const getUser = createServerFn({",
            "  method: \"GET\",",
            "})",
            "  .middleware([auth]) // trailing ) comment",
            "  .validator((s) => {",
            "    if (!/^[(]/.test(s.name) && s.label !== \")((\") return s;",
            "    throw new Error(`bad ${s.name}; (unbalanced`);",
            "  })",
            "  .handler(async ({ data }) => data);",
            "",
        ]
        .join("\n");
        let names: Vec<_> = find_server_fns(&code).into_iter().map(|f| f.name).collect();
        assert_eq!(names, ["getUser"]);
        let out = client(&code, "src/user.ts");
        assert!(
            out.contains(&format!(".handler({});", rpc("src/user.ts", "getUser"))),
            "{out}"
        );
        // stripMethodCall: the validator never reaches the browser.
        assert!(
            !out.contains("validator") && !out.contains("unbalanced"),
            "{out}"
        );
        assert!(out.contains(".middleware([auth])"), "{out}");
        assert_eq!(lines(&out), lines(&code));
        parses(&out, "src/user.ts");
    }

    #[test]
    fn input_validator_is_stripped_like_validator() {
        let code =
            "export const v = createServerFn().inputValidator((d) => d).handler(async () => 1);\n";
        let out = client(code, "src/v.ts");
        assert!(
            out.contains(&format!(
                "createServerFn().handler({});",
                rpc("src/v.ts", "v")
            )),
            "{out}"
        );
    }

    // The client and the server must agree on the set, so neither guesses.
    #[test]
    fn split_statement_factories_are_left_alone_on_both_sides() {
        let split = "const factory = createServerFn({ method: \"POST\" });\nexport const doIt = factory.handler(async () => 1);\n";
        assert_eq!(rewrite_client(split, "src/split.ts"), Ok(None));
        assert_eq!(rewrite_server(split, "src/split.ts"), None);
        let inline = "const factory = createServerFn({ method: \"POST\" }); export const doIt = factory.handler(async () => 1);\n";
        assert_eq!(rewrite_server(inline, "src/split.ts"), None);
        let asi = "const factory = createServerFn({ method: \"POST\" })\nexport const doIt = factory.handler(async () => 1)\n";
        assert_eq!(rewrite_server(asi, "src/asi.ts"), None);

        let mixed = "const factory = createServerFn({ method: \"POST\" });\nexport const real = createServerFn({ method: \"GET\" }).handler(async () => 2);\nexport const viaFactory = factory.handler(async () => 3);\n";
        let names: Vec<_> = find_server_fns(mixed).into_iter().map(|f| f.name).collect();
        assert_eq!(names, ["real"]);
        assert!(client(mixed, "src/mixed.ts").contains(".handler(async () => 3)"));
        let server = rewrite_server(mixed, "src/mixed.ts").unwrap();
        assert!(server.contains(".handler(real_createServerFn_handler, async () => 2)"));
        assert!(
            !server.contains("factory_createServerFn_handler"),
            "{server}"
        );
        assert!(server.contains(".handler(async () => 3)"), "{server}");
    }

    #[test]
    fn a_commented_out_server_fn_is_not_one() {
        let code =
            "// export const old = createServerFn().handler(async () => 1);\nexport const y = 2;\n";
        assert!(find_server_fns(code).is_empty());
        assert_eq!(rewrite_client(code, "src/y.ts"), Ok(None));
    }

    #[test]
    fn the_server_rewrite_adds_the_provider_export_without_lines() {
        let code = "import { createServerFn } from \"@tanstack/react-start\";\nexport const getGreeting = createServerFn({ method: \"GET\" }).handler(async () => \"hi\");\n";
        let out = rewrite_server(code, "src/server/data.ts").unwrap();
        let id = server_fn_id("src/server/data.ts", "getGreeting");
        assert!(out
            .starts_with("import { createServerRpc } from \"@tanstack/react-start/server-rpc\"; "));
        assert!(
            out.contains(&format!(
                "export const getGreeting_createServerFn_handler = createServerRpc({{ id: \"{id}\", name: \"getGreeting\", filename: \"src/server/data.ts\" }}, (opts) => getGreeting.__executeServer(opts)); "
            )),
            "{out}"
        );
        assert!(out.contains(".handler(getGreeting_createServerFn_handler, async () => \"hi\")"));
        assert_eq!(lines(&out), lines(code));
    }

    #[test]
    fn adjacent_server_fns_rewrite_independently() {
        let code = "export const a = createServerFn({ method: \"GET\" }).handler(async () => 1);\nexport const b = createServerFn({ method: \"POST\" }).handler(async () => 2);\n";
        let out = rewrite_server(code, "src/two.ts").unwrap();
        for name in ["a", "b"] {
            let id = server_fn_id("src/two.ts", name);
            assert!(
                out.contains(&format!(
                    "export const {name}_createServerFn_handler = createServerRpc({{ id: \"{id}\""
                )),
                "{out}"
            );
        }
        assert!(out.contains(".handler(a_createServerFn_handler, async () => 1)"));
        assert!(out.contains(".handler(b_createServerFn_handler, async () => 2)"));
        assert_eq!(lines(&out), lines(code));
        let client_out = client(code, "src/two.ts");
        assert!(client_out.contains(&rpc("src/two.ts", "a")));
        assert!(client_out.contains(&rpc("src/two.ts", "b")));
    }

    #[test]
    fn the_server_rewrite_keeps_a_commented_handler_body_intact() {
        let code =
            "export const k = createServerFn().handler(async () => {\n  // 1) chave\n  return 1;\n});\n";
        let out = rewrite_server(code, "src/k.ts").unwrap();
        assert!(
            out.contains(
                ".handler(k_createServerFn_handler, async () => {\n  // 1) chave\n  return 1;\n});"
            ),
            "{out}"
        );
    }

    #[test]
    fn modules_without_a_server_fn_pass_through() {
        assert_eq!(rewrite_client("export const x = 1;", "src/x.ts"), Ok(None));
        assert_eq!(rewrite_server("export const x = 1;", "src/x.ts"), None);
    }

    // A text scan cannot delimit these handler bodies: a regex after a keyword,
    // a `(` in JSX text. It gave up, and the handler shipped to the browser.
    #[test]
    fn handler_bodies_a_text_scan_cannot_delimit_are_still_replaced() {
        let cases = [
            (
                "src/re.ts",
                "re",
                "export const re = createServerFn().handler(async () => {\n  return /\\)/.test(\"x\");\n});\n",
            ),
            (
                "src/th.ts",
                "th",
                "export const th = createServerFn().handler(async ({ data }) => {\n  if (data) throw /x\\(/;\n  return 1;\n});\n",
            ),
            (
                "src/jsx.tsx",
                "jsx",
                "export const jsx = createServerFn().handler(async () => {\n  return <p>(note</p>;\n});\n",
            ),
        ];
        for (rel, name, code) in cases {
            let out = client(code, rel);
            assert!(
                out.contains(&format!(".handler({}", rpc(rel, name))),
                "{out}"
            );
            for gone in ["return", "throw", "<p>"] {
                assert!(!out.contains(gone), "{gone} left in:\n{out}");
            }
            assert_eq!(lines(&out), lines(code), "{out}");
        }
    }

    // TanStack unshifts its import into the program body, after the directives.
    #[test]
    fn the_import_lands_after_the_directive_prologue_on_its_line() {
        let code = "\"use client\";\nexport const d = createServerFn().handler(async () => 1);\n";
        let out = client(code, "src/d.ts");
        assert!(
            out.starts_with(&format!("\"use client\"; {CLIENT_IMPORT}")),
            "{out}"
        );
        assert_eq!(lines(&out), lines(code));
        let hashbang =
            "#!/usr/bin/env node\nexport const h = createServerFn().handler(async () => 1);\n";
        let out = client(hashbang, "src/h.ts");
        assert!(
            out.starts_with(&format!("#!/usr/bin/env node\n{CLIENT_IMPORT}")),
            "{out}"
        );
    }

    // Never left in place: an unrewritten server fn would ship its handler.
    #[test]
    fn a_server_fn_the_client_rewrite_cannot_handle_fails_loudly() {
        let err = rewrite_client("export const e = createServerFn().handler();\n", "src/e.ts")
            .unwrap_err();
        assert!(
            err.0.contains("could not rewrite server function e"),
            "{err}"
        );
    }

    // Byte offsets, not UTF-16: a multi-byte character before the handler
    // must not shift the matched call.
    #[test]
    fn multi_byte_source_before_the_handler_keeps_the_edit_aligned() {
        let code = "// ação — 🚀\nexport const u = createServerFn().handler(async () => \"é\");\n";
        let out = client(code, "src/u.ts");
        assert!(
            out.contains(&format!(".handler({});", rpc("src/u.ts", "u"))),
            "{out}"
        );
        parses(&out, "src/u.ts");
    }
}
