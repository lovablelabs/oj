use crate::scan::{scan, F_IMPORT_META_ENV};
use oxc_transformer_plugins::ReplaceGlobalDefinesConfig;
use std::sync::{Arc, LazyLock, RwLock};

/// What a compile derives from one resolved define list, built once per
/// `set_*` call rather than per compile: the plain-text gate keys and the oxc
/// config (parsed once, `Clone` over an `Arc`).
pub(crate) struct EnvDefines {
    /// Keys outside `import.meta.env*`, which the SIMD `F_IMPORT_META_ENV`
    /// scan already gates; these are the only scalar scans.
    plain_keys: Vec<String>,
    /// `None` when oxc rejected the list; compiles then skip the replacer.
    config: Option<ReplaceGlobalDefinesConfig>,
}

impl EnvDefines {
    fn build(pairs: Vec<(String, String)>) -> Arc<Self> {
        let plain_keys = pairs
            .iter()
            // Other import.meta.* defines (import.meta.vitest) are not covered
            // by the SIMD finder, so they stay plain keys.
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

/// The raw define lists and both compile variants derived from them.
#[derive(Default)]
struct EnvState {
    /// `None` until set; compiles then use the mode-derived fallback.
    client_pairs: Option<Vec<(String, String)>>,
    /// `environments.ssr.define`, layered over the shared list for SSR
    /// compiles only, so a key defined differently per side keeps both values.
    ssr_overrides: Vec<(String, String)>,
    client: Option<Arc<EnvDefines>>,
    ssr: Option<Arc<EnvDefines>>,
}

impl EnvState {
    /// Rederives both variants. Config parsing costs ~100us for 40 vars, so it
    /// runs in the setters, off the compile path.
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

/// The dev server's `import.meta.env` / `define` replacements, shared with
/// every compile through [`CompileOptions::env`](crate::CompileOptions).
/// Re-set when plugin `config()` hooks change env; setters rebuild both
/// variants, so call them at boot, not per request.
#[derive(Default)]
pub struct ImportMetaEnv {
    state: RwLock<EnvState>,
}

impl std::fmt::Debug for ImportMetaEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ImportMetaEnv")
    }
}

impl ImportMetaEnv {
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, EnvState> {
        self.state.write().unwrap_or_else(|e| e.into_inner())
    }

    /// The shared define list (client and SSR).
    pub fn set(&self, defines: Vec<(String, String)>) {
        let mut state = self.write();
        state.client_pairs = Some(defines);
        state.rebuild();
    }

    /// Replaces the SSR-only overrides.
    pub fn set_ssr(&self, overrides: Vec<(String, String)>) {
        let mut state = self.write();
        state.ssr_overrides = overrides;
        state.rebuild();
    }

    /// Folds more SSR overrides over the current ones (later wins): the
    /// resolved-config defines arrive when the lazy SSR plugin host spawns.
    pub fn merge_ssr(&self, overrides: Vec<(String, String)>) {
        let mut state = self.write();
        for (k, v) in overrides {
            if let Some(slot) = state.ssr_overrides.iter_mut().find(|(ek, _)| *ek == k) {
                slot.1 = v;
            } else {
                state.ssr_overrides.push((k, v));
            }
        }
        state.rebuild();
    }

    fn variant(&self, ssr: bool) -> Option<Arc<EnvDefines>> {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        if ssr { &state.ssr } else { &state.client }.clone()
    }
}

/// Mode-derived defines for compiles without a set [`ImportMetaEnv`] (`oj build`,
/// unit tests), one per (dev, ssr) variant, indexed by `fallback_idx`.
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

/// The defines a compile applies: `env`'s variant once set, else the fallback.
pub(crate) fn defines_for(env: Option<&ImportMetaEnv>, dev: bool, ssr: bool) -> Arc<EnvDefines> {
    env.and_then(|e| e.variant(ssr))
        .unwrap_or_else(|| Arc::clone(&FALLBACK_DEFINES[fallback_idx(dev, ssr)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    use crate::compile::{compile, CompileOptions};

    #[test]
    fn fallback_variants_are_cached_per_dev_ssr() {
        // With no env set, the same (dev, ssr) returns the same Arc and each
        // variant is its own entry.
        assert!(Arc::ptr_eq(
            &defines_for(None, true, false),
            &defines_for(None, true, false)
        ));
        assert!(Arc::ptr_eq(
            &defines_for(None, false, true),
            &defines_for(None, false, true)
        ));
        for (a, b) in [
            ((true, false), (true, true)),
            ((true, false), (false, false)),
            ((false, true), (true, true)),
            ((false, false), (false, true)),
        ] {
            assert!(
                !Arc::ptr_eq(&defines_for(None, a.0, a.1), &defines_for(None, b.0, b.1)),
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
