//! oj's own settings read from the environment, parsed once.

use std::ffi::OsStr;
use std::path::PathBuf;

/// Every `OJ_*` (and related) variable oj reads, parsed with the rules its
/// consumer used before. Raw strings are kept where the consumer parses them
/// itself (the `*_from` timeout helpers).
#[derive(Debug, Clone, Default)]
pub struct Knobs {
    /// The shell's `NODE_ENV`, when set and non-empty.
    pub node_env: Option<String>,
    /// The shell's `BABEL_ENV`, when set and non-empty (StyleX's `dev` default).
    pub babel_env: Option<String>,
    /// `NODE_OPTIONS`, for its `--max-old-space-size`.
    pub node_options: Option<String>,
    /// `NO_COLOR` set to anything (even empty).
    pub no_color: bool,
    /// `BROWSER`, when not blank; `none` disables opening.
    pub browser: Option<String>,
    /// `OJ_BOOT_PHASES` set: `[oj-phase]` timing logs.
    pub boot_phases: bool,

    /// `OJ_CACHE_DIR`, when non-empty: the cache base instead of `<app>/.oj-cache`.
    pub cache_dir: Option<PathBuf>,

    /// `OJ_HMR_GATE` or `LOVABLE_DEV_SERVER` is `1` / `true`.
    pub hmr_gate: bool,
    /// `OJ_HMR_FULL_RELOAD` (else `LOVABLE_HMR_FULL_RELOAD`) is not `false`.
    pub hmr_full_reload: bool,
    /// `OJ_MEMORY_CACHE_MB` as an integer (`0` = unlimited, the consumer's rule).
    pub memory_cache_mb: Option<usize>,
    /// `OJ_CODE_CACHE_MAX_BYTES` as an integer (`0` = unbounded; unset = the
    /// consumer's default budget).
    pub code_cache_max_bytes: Option<u64>,
    /// `OJ_LOG_JSON` truthy: diagnostics print as NDJSON lines on stderr.
    pub log_json: bool,
    /// `OJ_FORWARD_CONSOLE` as an override for `server.forwardConsole`:
    /// `1`/`true` on, `0`/`false` off, anything else unset.
    pub forward_console: Option<bool>,
    /// An AI-agent environment, per the env variables Vite's `determineAgent`
    /// checks (`server.forwardConsole` defaults on under one).
    pub agent_env: bool,
    /// `OJ_DEBUG_MEM` truthy.
    pub debug_mem: bool,
    /// `OJ_DEBUG_HOOK_GATE` is exactly `1`.
    pub debug_hook_gate: bool,

    /// `OJ_PLUGIN_MEMORY_MB`, when positive.
    pub plugin_memory_mb: Option<usize>,

    /// `OJ_START_MAX_BODY` bytes.
    pub start_max_body: Option<usize>,

    /// `OJ_STYLEX_NATIVE_SCOPE=1`: StyleX binding analysis from oj_stylex's own
    /// walk instead of oxc_semantic.
    pub stylex_native_scope: bool,
    /// `STYLEX_STAGE_TIMINGS=1`: per-stage StyleX compile timers.
    pub stylex_stage_timings: bool,
    /// `STYLEX_CACHE_DIR`, when non-empty: the `oj stylex` compile cache.
    pub stylex_cache_dir: Option<PathBuf>,
    /// `STYLEX_CACHE=0`: the `oj stylex` compile cache off, over any dir.
    pub stylex_cache_off: bool,
}

impl Knobs {
    pub(crate) fn parse<'a>(get: impl Fn(&str) -> Option<&'a OsStr>) -> Self {
        let var = |key: &str| get(key).and_then(OsStr::to_str).map(str::to_owned);
        // Set, non-empty and not "0".
        let truthy = |key: &str| var(key).is_some_and(|v| !v.is_empty() && v != "0");
        let one_or_true = |key: &str| matches!(var(key).as_deref(), Some("1") | Some("true"));
        let trimmed = |key: &str| var(key).and_then(|v| v.trim().parse::<u64>().ok());
        Knobs {
            node_env: var("NODE_ENV").filter(|v| !v.is_empty()),
            babel_env: var("BABEL_ENV").filter(|v| !v.is_empty()),
            node_options: var("NODE_OPTIONS"),
            no_color: get("NO_COLOR").is_some(),
            browser: var("BROWSER").filter(|v| !v.trim().is_empty()),
            boot_phases: get("OJ_BOOT_PHASES").is_some(),
            cache_dir: get("OJ_CACHE_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            hmr_gate: one_or_true("OJ_HMR_GATE") || one_or_true("LOVABLE_DEV_SERVER"),
            hmr_full_reload: var("OJ_HMR_FULL_RELOAD")
                .or_else(|| var("LOVABLE_HMR_FULL_RELOAD"))
                .as_deref()
                != Some("false"),
            memory_cache_mb: var("OJ_MEMORY_CACHE_MB").and_then(|v| v.trim().parse().ok()),
            code_cache_max_bytes: var("OJ_CODE_CACHE_MAX_BYTES")
                .and_then(|v| v.trim().parse().ok()),
            log_json: truthy("OJ_LOG_JSON"),
            forward_console: match var("OJ_FORWARD_CONSOLE").as_deref() {
                Some("1") | Some("true") => Some(true),
                Some("0") | Some("false") => Some(false),
                _ => None,
            },
            agent_env: [
                "AI_AGENT",
                "CURSOR_TRACE_ID",
                "CURSOR_AGENT",
                "GEMINI_CLI",
                "CODEX_SANDBOX",
                "CODEX_CI",
                "CODEX_THREAD_ID",
                "ANTIGRAVITY_AGENT",
                "AUGMENT_AGENT",
                "OPENCODE_CLIENT",
                "CLAUDECODE",
                "CLAUDE_CODE",
                "REPL_ID",
                "COPILOT_MODEL",
                "COPILOT_ALLOW_ALL",
                "COPILOT_GITHUB_TOKEN",
            ]
            .iter()
            .any(|key| var(key).is_some_and(|v| !v.trim().is_empty()))
                || var("CURSOR_EXTENSION_HOST_ROLE").as_deref() == Some("agent-exec"),
            debug_mem: truthy("OJ_DEBUG_MEM"),
            debug_hook_gate: var("OJ_DEBUG_HOOK_GATE").as_deref() == Some("1"),
            plugin_memory_mb: trimmed("OJ_PLUGIN_MEMORY_MB")
                .map(|v| v as usize)
                .filter(|v| *v > 0),
            start_max_body: var("OJ_START_MAX_BODY").and_then(|v| v.trim().parse().ok()),
            stylex_native_scope: var("OJ_STYLEX_NATIVE_SCOPE").as_deref() == Some("1"),
            stylex_stage_timings: var("STYLEX_STAGE_TIMINGS").as_deref() == Some("1"),
            stylex_cache_dir: get("STYLEX_CACHE_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            stylex_cache_off: var("STYLEX_CACHE").as_deref() == Some("0"),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Env;

    fn knobs(vars: &[(&str, &str)]) -> super::Knobs {
        Env::from_vars(vars.iter().map(|(k, v)| (*k, *v))).knobs
    }

    #[test]
    fn flags_follow_their_consumers_rules() {
        let k = knobs(&[]);
        assert!(!k.no_color && !k.boot_phases && !k.hmr_gate);
        assert!(k.hmr_full_reload, "full reload is the default");
        assert_eq!(k.node_env, None);

        // Truthy: set, non-empty and not "0".
        assert!(knobs(&[("OJ_LOG_JSON", "yes")]).log_json);
        assert!(!knobs(&[("OJ_LOG_JSON", "")]).log_json);
        assert!(knobs(&[("OJ_LOG_JSON", "1")]).log_json);
        assert!(!knobs(&[("OJ_LOG_JSON", "0")]).log_json);
        assert!(!knobs(&[]).log_json);
        assert_eq!(knobs(&[]).forward_console, None);
        assert_eq!(
            knobs(&[("OJ_FORWARD_CONSOLE", "true")]).forward_console,
            Some(true)
        );
        assert_eq!(
            knobs(&[("OJ_FORWARD_CONSOLE", "0")]).forward_console,
            Some(false)
        );
        assert_eq!(
            knobs(&[("OJ_FORWARD_CONSOLE", "yes")]).forward_console,
            None
        );
        // Vite's determineAgent env list; an empty value does not count.
        assert!(!knobs(&[]).agent_env);
        assert!(knobs(&[("AI_AGENT", "lovable")]).agent_env);
        assert!(knobs(&[("CLAUDECODE", "1")]).agent_env);
        assert!(!knobs(&[("AI_AGENT", " ")]).agent_env);
        assert!(knobs(&[("CURSOR_EXTENSION_HOST_ROLE", "agent-exec")]).agent_env);
        assert!(!knobs(&[("CURSOR_EXTENSION_HOST_ROLE", "ui")]).agent_env);
        // Presence: any value, even empty.
        assert!(knobs(&[("NO_COLOR", "")]).no_color);
        assert!(knobs(&[("OJ_BOOT_PHASES", "")]).boot_phases);
        // Exactly "1" / "1" or "true".
        assert!(!knobs(&[("OJ_DEBUG_HOOK_GATE", "true")]).debug_hook_gate);
        assert!(knobs(&[("OJ_DEBUG_HOOK_GATE", "1")]).debug_hook_gate);
        assert!(knobs(&[("LOVABLE_DEV_SERVER", "true")]).hmr_gate);
        assert!(!knobs(&[("OJ_HMR_GATE", "yes")]).hmr_gate);
        // The first full-reload variable wins; only "false" turns it off.
        assert!(!knobs(&[("OJ_HMR_FULL_RELOAD", "false")]).hmr_full_reload);
        assert!(
            knobs(&[
                ("OJ_HMR_FULL_RELOAD", ""),
                ("LOVABLE_HMR_FULL_RELOAD", "false")
            ])
            .hmr_full_reload
        );
        assert!(!knobs(&[("LOVABLE_HMR_FULL_RELOAD", "false")]).hmr_full_reload);
        // StyleX: exactly "1" selects the native scope walk; only "0" kills
        // the CLI cache.
        assert!(knobs(&[("OJ_STYLEX_NATIVE_SCOPE", "1")]).stylex_native_scope);
        assert!(!knobs(&[("OJ_STYLEX_NATIVE_SCOPE", "true")]).stylex_native_scope);
        assert!(!knobs(&[("FRU_NATIVE_SCOPES", "1")]).stylex_native_scope);
        assert!(knobs(&[("STYLEX_CACHE", "0")]).stylex_cache_off);
        assert!(!knobs(&[("STYLEX_CACHE", "false")]).stylex_cache_off);
        assert_eq!(knobs(&[("STYLEX_CACHE_DIR", "")]).stylex_cache_dir, None);
    }

    #[test]
    fn values_parse_like_their_consumers() {
        assert_eq!(knobs(&[("NODE_ENV", "")]).node_env, None);
        assert_eq!(
            knobs(&[("NODE_ENV", "production")]).node_env.as_deref(),
            Some("production")
        );
        assert_eq!(knobs(&[("BROWSER", "  ")]).browser, None);
        assert_eq!(knobs(&[("OJ_CACHE_DIR", "")]).cache_dir, None);
        assert_eq!(
            knobs(&[("OJ_PLUGIN_MEMORY_MB", "0")]).plugin_memory_mb,
            None
        );
        assert_eq!(
            knobs(&[("OJ_MEMORY_CACHE_MB", " 0 ")]).memory_cache_mb,
            Some(0)
        );
        assert_eq!(knobs(&[]).code_cache_max_bytes, None);
        assert_eq!(
            knobs(&[("OJ_CODE_CACHE_MAX_BYTES", " 1048576 ")]).code_cache_max_bytes,
            Some(1_048_576)
        );
        assert_eq!(
            knobs(&[("OJ_CODE_CACHE_MAX_BYTES", "0")]).code_cache_max_bytes,
            Some(0)
        );
    }
}
