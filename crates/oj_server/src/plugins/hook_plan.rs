/// One hook's build gate: presence, always-offered plugins, and include filters.
/// Only over-approximates: `wants` may say yes wrongly, never no for a module a plugin would claim.
#[derive(Debug, Default, Clone)]
pub struct HookFilterPlan {
    pub present: bool,
    pub unfiltered: bool,
    pub plugins: Vec<PluginFilter>,
}

#[derive(Debug, Clone)]
pub struct PluginFilter {
    pub id: Vec<HookFilter>,
    pub code: Vec<HookFilter>,
}

/// One `filter` pattern as the host's gatePatterns hands it over: a Rust
/// regex source (`{re}`), Vite's cwd-joined picomatch glob for a string id
/// (`{glob}`), or `{any}` for one it could not carry. A pattern that does not
/// compile here matches everything: the gate may over-match, never under.
#[derive(Debug, Clone)]
pub enum HookFilter {
    Regex(regex::Regex),
    Glob(std::sync::Arc<oj_compiler::glob::GlobMatcher>),
    Any,
}

impl HookFilter {
    pub fn from_json(v: &serde_json::Value) -> HookFilter {
        if let Some(re) = v.get("re").and_then(|s| s.as_str()) {
            return regex::Regex::new(re).map_or(HookFilter::Any, HookFilter::Regex);
        }
        if let Some(glob) = v.get("glob").and_then(|s| s.as_str()) {
            return oj_compiler::glob::GlobMatcher::plugin_id_filter(glob)
                .map_or(HookFilter::Any, |m| {
                    HookFilter::Glob(std::sync::Arc::new(m))
                });
        }
        HookFilter::Any
    }

    /// A JSON array of patterns.
    pub fn list_from_json(text: &str) -> Vec<HookFilter> {
        serde_json::from_str::<Vec<serde_json::Value>>(text)
            .map(|items| items.iter().map(HookFilter::from_json).collect())
            .unwrap_or_default()
    }

    pub fn is_match(&self, input: &str) -> bool {
        match self {
            HookFilter::Regex(re) => re.is_match(input),
            HookFilter::Glob(glob) => glob.is_match(input),
            HookFilter::Any => true,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct BuildHookPlan {
    pub transform: HookFilterPlan,
    pub load: HookFilterPlan,
    pub resolve_id: HookFilterPlan,
}

impl HookFilterPlan {
    fn fail_open() -> Self {
        Self {
            present: true,
            unfiltered: true,
            plugins: Vec::new(),
        }
    }

    pub(crate) fn from_json(v: Option<&serde_json::Value>) -> Self {
        let Some(v) = v else {
            return Self::fail_open();
        };
        let present = v.get("present").and_then(|b| b.as_bool()).unwrap_or(true);
        let mut unfiltered = v
            .get("unfiltered")
            .and_then(|b| b.as_bool())
            .unwrap_or(true);
        let mut plugins = Vec::new();
        for entry in v
            .get("plugins")
            .and_then(|p| p.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[])
        {
            let compile = |key: &str| -> Option<Vec<HookFilter>> {
                let items = entry.get(key).and_then(|x| x.as_array())?;
                Some(items.iter().map(HookFilter::from_json).collect())
            };
            match (compile("id"), compile("code")) {
                (Some(id), Some(code)) if !id.is_empty() || !code.is_empty() => {
                    plugins.push(PluginFilter { id, code });
                }
                _ => unfiltered = true,
            }
        }
        Self {
            present,
            unfiltered,
            plugins,
        }
    }

    /// Whether any plugin's filter could claim this module; a code filter with
    /// no code available passes, keeping the gate an over-approximation.
    pub fn wants(&self, id: &str, code: Option<&str>) -> bool {
        if !self.present {
            return false;
        }
        if self.unfiltered {
            return true;
        }
        // The host matches slash-normalized ids, so normalize Windows paths
        // the same way or the gate under-matches.
        let id = if id.contains('\\') {
            std::borrow::Cow::Owned(id.replace('\\', "/"))
        } else {
            std::borrow::Cow::Borrowed(id)
        };
        let id = id.as_ref();
        self.plugins.iter().any(|p| {
            let id_ok = p.id.is_empty() || p.id.iter().any(|f| f.is_match(id));
            let code_ok = p.code.is_empty()
                || match code {
                    Some(c) => p.code.iter().any(|f| f.is_match(c)),
                    None => true,
                };
            id_ok && code_ok
        })
    }
}

/// OJ_DEBUG_HOOK_GATE=1: gates log/count skipped RPCs so a test can assert a
/// skip happened (output alone cannot, the host's filters produce identical bytes).
pub fn hook_gate_debug() -> bool {
    oj_env::get().knobs.debug_hook_gate
}

impl BuildHookPlan {
    pub fn fail_open() -> Self {
        Self {
            transform: HookFilterPlan::fail_open(),
            load: HookFilterPlan::fail_open(),
            resolve_id: HookFilterPlan::fail_open(),
        }
    }
}
