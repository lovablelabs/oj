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
    pub id: Vec<regex::Regex>,
    pub code: Vec<regex::Regex>,
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
            let compile = |key: &str| -> Option<Vec<regex::Regex>> {
                let mut out = Vec::new();
                for s in entry.get(key).and_then(|x| x.as_array())? {
                    // A JS regex the regex crate cannot compile (lookaround,
                    // backrefs) cannot gate; the plugin then always crosses.
                    out.push(regex::Regex::new(s.as_str()?).ok()?);
                }
                Some(out)
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
            let id_ok = p.id.is_empty() || p.id.iter().any(|re| re.is_match(id));
            let code_ok = p.code.is_empty()
                || match code {
                    Some(c) => p.code.iter().any(|re| re.is_match(c)),
                    None => true,
                };
            id_ok && code_ok
        })
    }
}

/// OJ_DEBUG_HOOK_GATE=1: gates log/count skipped RPCs so a test can assert a
/// skip happened (output alone cannot, the host's filters produce identical bytes).
pub fn hook_gate_debug() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("OJ_DEBUG_HOOK_GATE").is_ok_and(|v| v == "1"));
    *ON
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
