use crate::scan::{scan, F_IMPORT_META_ENV};
use oxc_transformer_plugins::ReplaceGlobalDefinesConfig;
use std::sync::{Arc, LazyLock, RwLock};

/// Everything a compile derives from one resolved define list, built once per
/// `set_*` call instead of once per compile: the non-`import.meta` keys that
/// gate the replacer on plain source text, and the oxc config, which validates
/// and parses every value in its own arena and is `Clone` over an `Arc`.
pub(crate) struct EnvDefines {
    /// Keys other than `import.meta*`; `import.meta.env` itself is gated by the
    /// SIMD `F_IMPORT_META_ENV` scan, so these are the only scalar scans left.
    plain_keys: Vec<String>,
    /// `None` when oxc rejected the list, which skips the replacer for that
    /// compile (unchanged from the uncached behaviour).
    config: Option<ReplaceGlobalDefinesConfig>,
}

impl EnvDefines {
    fn build(pairs: Vec<(String, String)>) -> Arc<Self> {
        let plain_keys = pairs
            .iter()
            // Only import.meta.env itself (and its members) is covered by the
            // SIMD finder; any other import.meta.* define (import.meta.vitest)
            // must stay a scanned plain key or needed_by never sees it.
            .filter(|(k, _)| k != "import.meta.env" && !k.starts_with("import.meta.env."))
            .map(|(k, _)| k.clone())
            .collect();
        let config = ReplaceGlobalDefinesConfig::new(&pairs).ok();
        Arc::new(Self { plain_keys, config })
    }

    /// True when `source_text` can mention any define: `import.meta.env` via
    /// the SIMD finder, other keys via a scalar scan each.
    pub(crate) fn needed_by(&self, source_text: &str) -> bool {
        scan(&F_IMPORT_META_ENV, source_text)
            || self
                .plain_keys
                .iter()
                .any(|k| source_text.contains(k.as_str()))
    }

    /// The prebuilt replacer config (an `Arc` bump to clone), or `None` when
    /// oxc rejected the define list.
    pub(crate) fn config(&self) -> Option<ReplaceGlobalDefinesConfig> {
        self.config.clone()
    }
}

/// Server-provided defines: the raw inputs and both compile variants derived
/// from them. Variants are rebuilt eagerly in the setters (a few times per
/// process) so `import_meta_env_defines` is a read lock and an `Arc` clone.
struct EnvState {
    /// `set_import_meta_env`; `None` until the server sets it (the `oj build`
    /// path and unit tests never do, and use the mode-derived fallback).
    client_pairs: Option<Vec<(String, String)>>,
    /// `environments.ssr.define`: layered over the shared list for SSR
    /// compiles only, so a key defined differently per side keeps both values.
    ssr_overrides: Vec<(String, String)>,
    client: Option<Arc<EnvDefines>>,
    ssr: Option<Arc<EnvDefines>>,
}

impl EnvState {
    /// Rederives both variants from the raw inputs. Runs
    /// `ReplaceGlobalDefinesConfig::new` (an oxc parse of every value) twice on
    /// the caller's thread, roughly 100 us with 40 vars, so it belongs in the
    /// setters, not on the compile path.
    fn rebuild(&mut self) {
        let Some(pairs) = &self.client_pairs else {
            self.client = None;
            self.ssr = None;
            return;
        };
        let mut ssr = pairs.clone();
        for (k, v) in ssr.iter_mut() {
            if k == "import.meta.env.SSR" {
                *v = "true".into();
            } else if k == "import.meta.env" {
                *v = v.replace("\"SSR\":false", "\"SSR\":true");
            }
        }
        for (k, v) in &self.ssr_overrides {
            if let Some(slot) = ssr.iter_mut().find(|(ek, _)| ek == k) {
                slot.1 = v.clone();
            } else {
                ssr.push((k.clone(), v.clone()));
            }
        }
        self.client = Some(EnvDefines::build(pairs.clone()));
        self.ssr = Some(EnvDefines::build(ssr));
    }
}

// RwLock, not OnceLock: the server re-sets these once the plugin host reports
// config()-hook env mutations, which land after the initial dotenv-based set.
static ENV_DEFINES: RwLock<EnvState> = RwLock::new(EnvState {
    client_pairs: None,
    ssr_overrides: Vec::new(),
    client: None,
    ssr: None,
});

// The three setters are boot-time calls: each rebuilds both compile variants
// under the write lock, so the next compile sees the new list. Do not call
// them per request.
pub fn set_import_meta_env(defines: Vec<(String, String)>) {
    let mut state = ENV_DEFINES.write().expect("ENV_DEFINES poisoned");
    state.client_pairs = Some(defines);
    state.rebuild();
}

pub fn set_import_meta_env_ssr(overrides: Vec<(String, String)>) {
    let mut state = ENV_DEFINES.write().expect("ENV_DEFINES poisoned");
    state.ssr_overrides = overrides;
    state.rebuild();
}

/// Folds more ssr-environment defines over the current set (later wins): the
/// resolved-config defines arrive when the lazy ssr plugin host spawns, after
/// the boot-time set from the oj config.
pub fn merge_import_meta_env_ssr(overrides: Vec<(String, String)>) {
    let mut state = ENV_DEFINES.write().expect("ENV_DEFINES poisoned");
    for (k, v) in overrides {
        if let Some(slot) = state.ssr_overrides.iter_mut().find(|(ek, _)| *ek == k) {
            slot.1 = v;
        } else {
            state.ssr_overrides.push((k, v));
        }
    }
    state.rebuild();
}

/// Mode-derived defines used until the server calls `set_import_meta_env`
/// (and always by `oj build` and the unit tests), one per (dev, ssr) variant.
/// Indexed by `fallback_idx(dev, ssr)`.
static FALLBACK_DEFINES: [LazyLock<Arc<EnvDefines>>; 4] = [
    LazyLock::new(|| fallback_defines(false, false)),
    LazyLock::new(|| fallback_defines(false, true)),
    LazyLock::new(|| fallback_defines(true, false)),
    LazyLock::new(|| fallback_defines(true, true)),
];

/// `dev << 1 | ssr`, the order of `FALLBACK_DEFINES`.
fn fallback_idx(dev: bool, ssr: bool) -> usize {
    ((dev as usize) << 1) | ssr as usize
}

fn fallback_defines(dev: bool, ssr: bool) -> Arc<EnvDefines> {
    let mode = if dev { "development" } else { "production" };
    EnvDefines::build(vec![
        ("import.meta.env.BASE_URL".into(), "\"/\"".into()),
        ("import.meta.env.MODE".into(), format!("\"{mode}\"")),
        ("import.meta.env.DEV".into(), dev.to_string()),
        ("import.meta.env.PROD".into(), (!dev).to_string()),
        ("import.meta.env.SSR".into(), ssr.to_string()),
        (
            "import.meta.env".into(),
            format!(
                "({{\"BASE_URL\":\"/\",\"MODE\":\"{mode}\",\"DEV\":{dev},\"PROD\":{prod},\"SSR\":{ssr}}})",
                prod = !dev
            ),
        ),
    ])
}

pub(crate) fn import_meta_env_defines(dev: bool, ssr: bool) -> Arc<EnvDefines> {
    {
        let state = ENV_DEFINES.read().expect("ENV_DEFINES poisoned");
        let variant = if ssr { &state.ssr } else { &state.client };
        if let Some(defines) = variant {
            return Arc::clone(defines);
        }
    }
    Arc::clone(&FALLBACK_DEFINES[fallback_idx(dev, ssr)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    use crate::compile::{compile, CompileOptions};

    #[test]
    fn fallback_variants_are_cached_per_dev_ssr() {
        // The unit tests never call set_import_meta_env, so every lookup lands
        // on the fallback table: the same (dev, ssr) hands back the same Arc,
        // and each variant is its own entry.
        assert!(Arc::ptr_eq(
            &import_meta_env_defines(true, false),
            &import_meta_env_defines(true, false)
        ));
        assert!(Arc::ptr_eq(
            &import_meta_env_defines(false, true),
            &import_meta_env_defines(false, true)
        ));
        for (a, b) in [
            ((true, false), (true, true)),
            ((true, false), (false, false)),
            ((false, true), (true, true)),
            ((false, false), (false, true)),
        ] {
            assert!(
                !Arc::ptr_eq(
                    &import_meta_env_defines(a.0, a.1),
                    &import_meta_env_defines(b.0, b.1)
                ),
                "{a:?} and {b:?} must be distinct fallback variants"
            );
        }
    }

    #[test]
    fn replaces_import_meta_env_flags_per_mode() {
        let src = "export const mode = import.meta.env.MODE;\n\
                   export const dev = import.meta.env.DEV;\n\
                   export const prod = import.meta.env.PROD;";
        let prod = compile(Path::new("env.ts"), src, &CompileOptions::prod()).unwrap();
        assert!(
            !prod.code.contains("import.meta.env"),
            "defines must be replaced:\n{}",
            prod.code
        );
        assert!(
            prod.code.contains("\"production\""),
            "MODE is production:\n{}",
            prod.code
        );
        assert!(
            prod.code.contains("prod = true"),
            "PROD is true in prod:\n{}",
            prod.code
        );
        assert!(
            prod.code.contains("dev = false"),
            "DEV is false in prod:\n{}",
            prod.code
        );

        let dev = compile(Path::new("env.ts"), src, &CompileOptions::dev()).unwrap();
        assert!(
            dev.code.contains("\"development\""),
            "MODE is development:\n{}",
            dev.code
        );
        assert!(
            dev.code.contains("dev = true"),
            "DEV is true in dev:\n{}",
            dev.code
        );
        assert!(
            dev.code.contains("prod = false"),
            "PROD is false in dev:\n{}",
            dev.code
        );
    }
}
