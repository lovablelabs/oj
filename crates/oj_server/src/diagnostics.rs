use super::*;

/// `GET /@oj/diagnostics`: the ring of recent diagnostic events (see
/// `oj_diag`) plus plugin-host health, for supervisors, agents and tests.
/// Always on, but refused for anything carrying an `Origin` header (a hostile
/// page's cross-origin fetch always does) or arriving through a proxy
/// (`x-forwarded-host`): the consumers are local tools hitting localhost
/// directly, and the event text names project paths and code frames that must
/// not be readable through a public preview URL.
/// `?after=<epoch_ms>` keeps only newer events (a poller's cursor).
pub(crate) async fn serve_diagnostics(
    headers: HeaderMap,
    uri: Uri,
    State(state): State<Arc<ServerState>>,
) -> Response {
    if headers.contains_key(header::ORIGIN) || headers.contains_key("x-forwarded-host") {
        return (StatusCode::FORBIDDEN, "").into_response();
    }
    let after = uri
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("after=")))
        .and_then(|v| v.parse::<u64>().ok());
    let mut body = oj_diag::global().snapshot(after);
    body["startedAt"] = state.started_at_ms.into();
    body["pluginHost"] = match &state.plugins {
        None => serde_json::json!({ "present": false }),
        Some(host) => serde_json::json!({
            "present": true,
            "initialized": host.is_initialized(),
            "gone": *host.host_gone_updates().borrow(),
            "canRevive": host.can_revive(),
        }),
    };
    (
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

const CLIENT_FIELD_CAP: usize = 4096;

fn truncated(s: &str, cap: usize) -> String {
    let mut end = s.len().min(cap);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Control characters in client-sourced text become spaces: an embedded
/// newline would let a page forge whole stderr lines (in NDJSON mode a forged
/// `"oj":"diag"` line would be trusted by a supervisor), and ANSI escapes
/// would reach the terminal.
fn scrubbed(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// A client string, bounded and printable on one stderr line.
fn client_text(s: &str, cap: usize) -> String {
    scrubbed(&truncated(s, cap))
}

/// A `vite:forward-console` payload from the HMR client (Vite 8's
/// forwardConsole channel): browser unhandled errors and forwarded console
/// lines. Printed to stderr like Vite's server does, with stack frames
/// remapped through the served modules' source maps, and recorded as
/// client-sourced diagnostics.
pub(crate) fn ingest_forward_console(state: &Arc<ServerState>, data: &serde_json::Value) {
    match data["type"].as_str() {
        Some(kind @ ("error" | "unhandled-rejection")) => {
            let err = &data["data"];
            let name = client_text(err["name"].as_str().unwrap_or("Error"), 128);
            let message = client_text(
                err["message"].as_str().unwrap_or_default(),
                CLIENT_FIELD_CAP,
            );
            let label = if kind == "unhandled-rejection" {
                "Unhandled rejection"
            } else {
                "Unhandled error"
            };
            let raw = truncated(
                err["stack"].as_str().unwrap_or_default(),
                CLIENT_FIELD_CAP * 4,
            );
            let (stack, module) = remap_stack(&raw, |url| {
                state
                    .memory
                    .lock()
                    .unwrap()
                    .peek(url)
                    .and_then(|m| m.map_json.clone())
            });
            // Scrubbed per line: the stack keeps its newlines (every line
            // below gets the indent), but nothing else control-shaped.
            let stack = stack.lines().map(scrubbed).collect::<Vec<_>>().join("\n");
            let indented: String = stack
                .lines()
                .map(|l| format!("\n    {}", l.trim()))
                .collect();
            eprintln!("oj: [browser] {label} {name}: {message}{indented}");
            oj_diag::emit(
                oj_diag::Event::new(
                    oj_diag::Kind::RuntimeError,
                    format!("{label} {name}: {message}"),
                )
                .source(oj_diag::Source::Client)
                .module(module)
                .detail(stack),
            );
        }
        Some("log") => {
            let level = client_text(data["data"]["level"].as_str().unwrap_or("log"), 32);
            let message = client_text(
                data["data"]["message"].as_str().unwrap_or_default(),
                CLIENT_FIELD_CAP,
            );
            eprintln!("oj: [browser console.{level}] {message}");
            if level == "error" || level == "warn" {
                let mut event = oj_diag::Event::new(
                    oj_diag::Kind::ConsoleError,
                    format!("console.{level}: {message}"),
                )
                .source(oj_diag::Source::Client);
                if level == "warn" {
                    event = event.warn();
                }
                oj_diag::emit(event);
            }
        }
        _ => {}
    }
}

/// An `oj:hmr-result` payload: the client failed to apply an update (a signal
/// Vite does not have; the server otherwise never learns an update was lost).
pub(crate) fn ingest_hmr_result(data: &serde_json::Value) {
    if data["ok"].as_bool() != Some(false) {
        return;
    }
    let path = client_text(data["path"].as_str().unwrap_or("?"), 1024);
    let error = client_text(data["error"].as_str().unwrap_or_default(), CLIENT_FIELD_CAP);
    eprintln!("oj: hmr update failed to apply in the browser for {path}: {error}");
    oj_diag::emit(
        oj_diag::Event::new(
            oj_diag::Kind::HmrApplyFailed,
            format!("update failed to apply: {error}"),
        )
        .source(oj_diag::Source::Client)
        .module(path),
    );
}

/// Remap a browser stack through the served modules' source maps (what Vite's
/// forwardConsole formatError does with its module graph): every
/// `http(s)://host/url:line:col` whose transform is cached maps back to the
/// original source and position; a frame that misses stays as sent. Returns
/// the remapped stack and the first remapped source as the event's module.
fn remap_stack(stack: &str, map_for: impl Fn(&str) -> Option<String>) -> (String, Option<String>) {
    static FRAME: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = FRAME.get_or_init(|| {
        regex::Regex::new(r"(https?://[^\s()]+?):(\d+):(\d+)").expect("frame regex")
    });
    let mut module = None;
    let mut maps: HashMap<String, Option<String>> = HashMap::new();
    let out = re.replace_all(stack, |c: &regex::Captures<'_>| {
        let full = &c[0];
        let (line, col) = match (c[2].parse::<u32>(), c[3].parse::<u32>()) {
            (Ok(l), Ok(col)) if l > 0 && col > 0 => (l, col),
            _ => return full.to_string(),
        };
        // The served path+query, origin dropped and the `t=` buster stripped:
        // the memory cache's key form.
        let Some(path_start) = c[1]
            .find("://")
            .and_then(|i| c[1][i + 3..].find('/').map(|j| i + 3 + j))
        else {
            return full.to_string();
        };
        let url = strip_hmr_timestamp(&c[1][path_start..]);
        let map_json = maps
            .entry(url.clone())
            .or_insert_with(|| map_for(&url))
            .clone();
        let Some(map_json) = map_json else {
            return full.to_string();
        };
        let Ok(sm) = oxc_sourcemap::SourceMap::from_json_string(&map_json) else {
            return full.to_string();
        };
        let table = sm.generate_lookup_table();
        let Some(token) = sm.lookup_token(&table, line - 1, col - 1) else {
            return full.to_string();
        };
        let source = token
            .get_source_id()
            .and_then(|id| sm.get_source(id))
            .unwrap_or(&url);
        if module.is_none() {
            module = Some(source.to_string());
        }
        format!(
            "{source}:{}:{}",
            token.get_src_line() + 1,
            token.get_src_col() + 1
        )
    });
    (out.into_owned(), module)
}

#[cfg(test)]
mod tests {
    use super::{client_text, remap_stack, truncated};

    /// A compile of a known source yields a map whose tokens remap a stack
    /// frame in the served output back to the original file and position.
    #[test]
    fn stacks_remap_through_the_served_module_map() {
        let src =
            "type N = number;\nexport function boom(n: N): N {\n  throw new Error(\"x\");\n}\n";
        let out = oj_compiler::compile(
            std::path::Path::new("/src/boom.ts"),
            src,
            &oj_compiler::CompileOptions::dev(),
        )
        .expect("compiles");
        let map_json = out.map_json.expect("dev compiles carry maps");
        // The compiled output drops the type-only line; find where the throw
        // landed so the fake frame points at real generated code.
        let gen_line = out
            .code
            .lines()
            .position(|l| l.contains("throw"))
            .expect("throw survives") as u32
            + 1;
        let col = out
            .code
            .lines()
            .nth(gen_line as usize - 1)
            .unwrap()
            .find("throw")
            .unwrap() as u32
            + 1;
        let stack = format!(
            "Error: x\n    at boom (http://localhost:5199/src/boom.ts?t=1791558244983:{gen_line}:{col})\n    at http://localhost:5199/src/other.ts:1:1\n"
        );
        let (remapped, module) = remap_stack(&stack, |url| {
            (url == "/src/boom.ts").then(|| map_json.clone())
        });
        assert_eq!(module.as_deref(), Some("/src/boom.ts"));
        assert!(
            remapped.contains("/src/boom.ts:3:3"),
            "the throw maps back to source line 3: {remapped}"
        );
        assert!(
            remapped.contains("http://localhost:5199/src/other.ts:1:1"),
            "an unmapped frame stays as sent: {remapped}"
        );
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncated("héllo", 2), "h");
        assert_eq!(truncated("hi", 10), "hi");
    }

    /// A page's newline or ANSI escape must not forge stderr lines (an NDJSON
    /// supervisor trusts whole lines) or color the terminal.
    #[test]
    fn client_text_neutralizes_control_characters() {
        assert_eq!(
            client_text("a\n{\"oj\":\"diag\"}\r\x1b[31mred", 4096),
            "a {\"oj\":\"diag\"}  [31mred"
        );
        assert_eq!(client_text("plain", 4096), "plain");
    }
}
