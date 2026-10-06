//! oj's own settings read from the environment, parsed once.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// Every `OJ_*` (and related) variable oj reads, parsed with the rules its
/// consumer used before. Raw strings are kept where the consumer parses them
/// itself (the `*_from` timeout helpers).
#[derive(Debug, Clone, Default)]
pub struct Knobs {
    /// The shell's `NODE_ENV`, when set and non-empty.
    pub node_env: Option<String>,
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
    /// `OJ_CACHE_VERIFY=full` (any case).
    pub cache_verify_full: bool,
    /// `OJ_VENDORED_ROLLDOWN`, raw (empty and non-UTF-8 are the consumer's call).
    pub vendored_rolldown: Option<OsString>,
    /// `OJ_ENABLE_CACHE` / `OJ_NO_CACHE` truthy (non-empty and not `0`).
    pub enable_cache: bool,
    pub no_cache: bool,
    /// `NODE_COMPILE_CACHE`, raw.
    pub node_compile_cache: Option<OsString>,
    /// `OJ_V8_COMPILE_CACHE`, raw.
    pub v8_compile_cache: Option<OsString>,

    /// `OJ_HMR_GATE` or `LOVABLE_DEV_SERVER` is `1` / `true`.
    pub hmr_gate: bool,
    /// `OJ_HMR_FULL_RELOAD` (else `LOVABLE_HMR_FULL_RELOAD`) is not `false`.
    pub hmr_full_reload: bool,
    /// `OJ_HMR_DEBOUNCE_MS` as an integer.
    pub hmr_debounce_ms: Option<u64>,
    /// `OJ_MEMORY_CACHE_MB` as an integer (`0` = unlimited, the consumer's rule).
    pub memory_cache_mb: Option<usize>,
    /// `OJ_DEBUG_MEM` truthy.
    pub debug_mem: bool,
    /// `OJ_DEBUG_HOOK_GATE` is exactly `1`.
    pub debug_hook_gate: bool,

    /// `OJ_NO_DEPS_PRESEED` truthy.
    pub no_deps_preseed: bool,
    /// `OJ_PRESEED_TIMEOUT` seconds, when positive.
    pub preseed_timeout_secs: Option<u64>,
    /// `OJ_OPTIMIZE_SCAN` truthy.
    pub optimize_scan: bool,
    /// Raw timeouts, parsed by the consumers' `*_from` helpers.
    pub optimize_timeout: Option<String>,
    pub plugin_timeout: Option<String>,
    pub plugin_init_timeout: Option<String>,
    pub extract_timeout: Option<String>,
    /// `OJ_PLUGIN_MEMORY_MB`, when positive.
    pub plugin_memory_mb: Option<usize>,

    /// `OJ_START_MAX_BODY` bytes.
    pub start_max_body: Option<usize>,
    /// `OJ_START_UNRESPONSIVE` seconds, when positive.
    pub start_unresponsive_secs: Option<u64>,
}

impl Knobs {
    pub(crate) fn parse<'a>(get: impl Fn(&str) -> Option<&'a OsStr>) -> Self {
        let var = |key: &str| get(key).and_then(OsStr::to_str).map(str::to_owned);
        let os = |key: &str| get(key).map(OsStr::to_os_string);
        // Set, non-empty and not "0".
        let truthy = |key: &str| var(key).is_some_and(|v| !v.is_empty() && v != "0");
        let one_or_true = |key: &str| matches!(var(key).as_deref(), Some("1") | Some("true"));
        let trimmed = |key: &str| var(key).and_then(|v| v.trim().parse::<u64>().ok());
        Knobs {
            node_env: var("NODE_ENV").filter(|v| !v.is_empty()),
            node_options: var("NODE_OPTIONS"),
            no_color: get("NO_COLOR").is_some(),
            browser: var("BROWSER").filter(|v| !v.trim().is_empty()),
            boot_phases: get("OJ_BOOT_PHASES").is_some(),
            cache_dir: get("OJ_CACHE_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            cache_verify_full: var("OJ_CACHE_VERIFY")
                .is_some_and(|v| v.eq_ignore_ascii_case("full")),
            vendored_rolldown: os("OJ_VENDORED_ROLLDOWN"),
            enable_cache: truthy("OJ_ENABLE_CACHE"),
            no_cache: truthy("OJ_NO_CACHE"),
            node_compile_cache: os("NODE_COMPILE_CACHE"),
            v8_compile_cache: os("OJ_V8_COMPILE_CACHE"),
            hmr_gate: one_or_true("OJ_HMR_GATE") || one_or_true("LOVABLE_DEV_SERVER"),
            hmr_full_reload: var("OJ_HMR_FULL_RELOAD")
                .or_else(|| var("LOVABLE_HMR_FULL_RELOAD"))
                .as_deref()
                != Some("false"),
            hmr_debounce_ms: var("OJ_HMR_DEBOUNCE_MS").and_then(|v| v.parse().ok()),
            memory_cache_mb: var("OJ_MEMORY_CACHE_MB").and_then(|v| v.trim().parse().ok()),
            debug_mem: truthy("OJ_DEBUG_MEM"),
            debug_hook_gate: var("OJ_DEBUG_HOOK_GATE").as_deref() == Some("1"),
            no_deps_preseed: truthy("OJ_NO_DEPS_PRESEED"),
            preseed_timeout_secs: trimmed("OJ_PRESEED_TIMEOUT").filter(|s| *s > 0),
            optimize_scan: truthy("OJ_OPTIMIZE_SCAN"),
            optimize_timeout: var("OJ_OPTIMIZE_TIMEOUT"),
            plugin_timeout: var("OJ_PLUGIN_TIMEOUT"),
            plugin_init_timeout: var("OJ_PLUGIN_INIT_TIMEOUT"),
            extract_timeout: var("OJ_EXTRACT_TIMEOUT"),
            plugin_memory_mb: trimmed("OJ_PLUGIN_MEMORY_MB")
                .map(|v| v as usize)
                .filter(|v| *v > 0),
            start_max_body: var("OJ_START_MAX_BODY").and_then(|v| v.trim().parse().ok()),
            start_unresponsive_secs: trimmed("OJ_START_UNRESPONSIVE").filter(|s| *s > 0),
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
        assert!(!k.enable_cache && !k.no_color && !k.boot_phases && !k.hmr_gate);
        assert!(k.hmr_full_reload, "full reload is the default");
        assert_eq!(k.node_env, None);

        // Truthy: set, non-empty and not "0".
        assert!(knobs(&[("OJ_ENABLE_CACHE", "1")]).enable_cache);
        assert!(knobs(&[("OJ_ENABLE_CACHE", "yes")]).enable_cache);
        assert!(!knobs(&[("OJ_ENABLE_CACHE", "0")]).enable_cache);
        assert!(!knobs(&[("OJ_ENABLE_CACHE", "")]).enable_cache);
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
        assert!(knobs(&[("OJ_CACHE_VERIFY", "FULL")]).cache_verify_full);
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
            knobs(&[("OJ_PRESEED_TIMEOUT", " 9 ")]).preseed_timeout_secs,
            Some(9)
        );
        assert_eq!(
            knobs(&[("OJ_PRESEED_TIMEOUT", "0")]).preseed_timeout_secs,
            None
        );
        assert_eq!(
            knobs(&[("OJ_PLUGIN_MEMORY_MB", "0")]).plugin_memory_mb,
            None
        );
        assert_eq!(
            knobs(&[("OJ_MEMORY_CACHE_MB", " 0 ")]).memory_cache_mb,
            Some(0)
        );
        // No trim for the debounce, as its consumer did.
        assert_eq!(knobs(&[("OJ_HMR_DEBOUNCE_MS", " 5")]).hmr_debounce_ms, None);
        assert_eq!(
            knobs(&[("OJ_HMR_DEBOUNCE_MS", "0")]).hmr_debounce_ms,
            Some(0)
        );
        assert_eq!(
            knobs(&[("OJ_PLUGIN_TIMEOUT", " 7 ")])
                .plugin_timeout
                .as_deref(),
            Some(" 7 ")
        );
    }
}
