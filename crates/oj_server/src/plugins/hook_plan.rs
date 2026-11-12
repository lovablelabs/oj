/// One hook's build gate: presence, always-offered plugins, and include filters.
/// Only over-approximates: `wants` may say yes wrongly, never no for a module a plugin would claim.
#[derive(Debug, Default, Clone)]
pub struct HookFilterPlan {
    pub present: bool,
    pub unfiltered: bool,
    /// An always-offered plugin Vite's container runs before `vite:asset`
    /// (a hook with `order: "pre"`, else an `enforce: "pre"` plugin): the
    /// ones whose `load` can claim a module vite:asset would answer.
    pub unfiltered_before_asset: bool,
    pub plugins: Vec<PluginFilter>,
}

#[derive(Debug, Clone)]
pub struct PluginFilter {
    pub id: Vec<HookFilter>,
    pub code: Vec<HookFilter>,
    /// See [`HookFilterPlan::unfiltered_before_asset`].
    pub before_asset: bool,
}

/// One `filter` pattern as the host's gatePatterns hands it over: a RegExp's
/// `{source, flags}`, a string code pattern as a `{literal}` (Vite's
/// `code.includes`), Vite's cwd-joined picomatch glob for a string id
/// (`{glob}`), or `{any}` for one it could not carry. A pattern that cannot be
/// carried here matches everything: the gate may over-match, never under.
#[derive(Debug, Clone)]
pub enum HookFilter {
    Regex(regex::Regex),
    Glob(std::sync::Arc<oj_compiler::glob::GlobMatcher>),
    Any,
}

impl HookFilter {
    pub fn from_json(v: &serde_json::Value) -> HookFilter {
        let text = |key: &str| v.get(key).and_then(|s| s.as_str());
        let regex = |re: &str| regex::Regex::new(re).map_or(HookFilter::Any, HookFilter::Regex);
        if let Some(source) = text("source") {
            return rust_regex_from_js(source, text("flags").unwrap_or(""))
                .map_or(HookFilter::Any, |re| regex(&re));
        }
        if let Some(literal) = text("literal") {
            return regex(&regex::escape(literal));
        }
        if let Some(glob) = text("glob") {
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
            unfiltered_before_asset: true,
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
        // A host without the order flags (older) is read as the open side.
        let mut unfiltered_before_asset = v
            .get("unfilteredBeforeAsset")
            .and_then(|b| b.as_bool())
            .unwrap_or(unfiltered);
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
            let before_asset = entry
                .get("beforeAsset")
                .and_then(|b| b.as_bool())
                .unwrap_or(true);
            match (compile("id"), compile("code")) {
                (Some(id), Some(code)) if !id.is_empty() || !code.is_empty() => {
                    plugins.push(PluginFilter {
                        id,
                        code,
                        before_asset,
                    });
                }
                _ => {
                    unfiltered = true;
                    if before_asset {
                        unfiltered_before_asset = true;
                    }
                }
            }
        }
        Self {
            present,
            unfiltered,
            unfiltered_before_asset,
            plugins,
        }
    }

    /// [`Self::wants`] for a module `vite:asset` would answer (a plain svg):
    /// only a plugin Vite's container runs before it can claim one, so only
    /// those are asked.
    pub fn wants_before_asset(&self, id: &str) -> bool {
        if !self.present {
            return false;
        }
        if self.unfiltered_before_asset {
            return true;
        }
        let id = normalized_id(id);
        self.plugins
            .iter()
            .filter(|p| p.before_asset)
            .any(|p| p.matches(&id, None))
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
        let id = normalized_id(id);
        self.plugins.iter().any(|p| p.matches(&id, code))
    }
}

impl PluginFilter {
    /// Whether this plugin's filter could claim the module; a code filter
    /// with no code available passes (an over-approximation).
    fn matches(&self, id: &str, code: Option<&str>) -> bool {
        let id_ok = self.id.is_empty() || self.id.iter().any(|f| f.is_match(id));
        let code_ok = self.code.is_empty()
            || match code {
                Some(c) => self.code.iter().any(|f| f.is_match(c)),
                None => true,
            };
        id_ok && code_ok
    }
}

/// The host matches slash-normalized ids, so a Windows path is normalized
/// the same way or the gate under-matches.
fn normalized_id(id: &str) -> std::borrow::Cow<'_, str> {
    if id.contains('\\') {
        std::borrow::Cow::Owned(id.replace('\\', "/"))
    } else {
        std::borrow::Cow::Borrowed(id)
    }
}

impl HookFilterPlan {
    /// Whether an id filter of a present, filtered hook matches `id` (see
    /// [`BuildHookPlan::names_module`]); a code-only filter names no id.
    pub fn names(&self, id: &str) -> bool {
        if !self.present || self.unfiltered {
            return false;
        }
        let id = if id.contains('\\') {
            std::borrow::Cow::Owned(id.replace('\\', "/"))
        } else {
            std::borrow::Cow::Borrowed(id)
        };
        self.plugins
            .iter()
            .any(|p| !p.id.is_empty() && p.id.iter().any(|f| f.is_match(&id)))
    }
}

/// `--debug hook-gate`: gates log/count skipped RPCs so a test can assert a
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

    /// Whether a plugin's `load` or `transform` filter names the module `id`
    /// outright: an id filter matches it (a function-form hook, which
    /// [`HookFilterPlan::wants`] offers every module, names none). What a
    /// plain asset import consults: Vite asks every `load` for it, and oj
    /// keeps its url module without a round trip unless a filter names the
    /// file, so a plugin that takes `.png` by filter is honoured and one that
    /// takes everything by shape costs no RPC per image.
    pub fn names_module(&self, id: &str) -> bool {
        self.load.names(id) || self.transform.names(id)
    }

    /// Whether a plugin's `load` or `transform` could claim the module `id`
    /// (a file path with the request's query, as the hooks see it) before
    /// its source is known: what decides that a request for a file of an
    /// extension oj does not compile, or an asset under a query oj does not
    /// know, goes through the plugin pipeline rather than being served as
    /// the file. Over-approximates like [`HookFilterPlan::wants`]: a code
    /// filter passes, since the code is what the pipeline would produce.
    pub fn claims_module(&self, id: &str) -> bool {
        self.load.wants(id, None) || self.transform.wants(id, None)
    }
}

/// JS's `\s`, its own whitespace set rather than Rust's Unicode one.
const JS_SPACE: &str =
    r"\t\n\x0B\f\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

/// Letters a JS escape gives a meaning without the u flag; any other escaped
/// letter is the letter itself.
const JS_LETTER_ESCAPES: &str = "bBdDwWsStnvfrcxuk";

/// A JS RegExp as a Rust regex that matches exactly where it does, or None
/// when it cannot be carried.
///
/// i/s change what the pattern matches and Rust takes them inline; g/y/d only
/// affect JS-side state or match metadata. m cannot be carried: JS multiline
/// anchors honor \r and \u2028, Rust's (?m) only \n. JS's perl classes are
/// ASCII (\b \w \d) and its \s is its own set, while Rust's are Unicode, so
/// they are spelled out; a class negation inside a set (`[\W]`) has no
/// spelling. Syntax Rust reads another way is spelled as JS reads it: a
/// letter escape JS takes literally (`\p`, `\A`, `\z`), and inside a set a
/// nested `[` and the `&&` `~~` operators. A `--` in a set (a JS range, a Rust
/// difference) and `\x{..}` / `\u{..}` (a letter and a quantifier in JS)
/// fail open. Any other JS-only syntax fails Rust's compile, and so fails open.
pub(crate) fn rust_regex_from_js(source: &str, flags: &str) -> Option<String> {
    if flags.chars().any(|f| !"isgyd".contains(f)) {
        return None;
    }
    let inline: String = flags.chars().filter(|f| matches!(f, 'i' | 's')).collect();
    let mut out = if inline.is_empty() {
        String::new()
    } else {
        format!("(?{inline})")
    };
    let chars: Vec<char> = source.chars().collect();
    let mut in_set = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if c == '\\' && i < chars.len() {
            let e = chars[i];
            i += 1;
            if matches!(e, 'x' | 'u') && chars.get(i) == Some(&'{') {
                return None;
            }
            let class = match e.to_ascii_lowercase() {
                'd' => Some("0-9"),
                'w' => Some("0-9A-Za-z_"),
                's' => Some(JS_SPACE),
                _ => None,
            };
            if e.is_ascii_alphabetic() && !JS_LETTER_ESCAPES.contains(e) {
                out.push(e);
            } else if let Some(class) = class {
                let negated = e.is_ascii_uppercase();
                if in_set {
                    if negated {
                        return None;
                    }
                    out.push_str(class);
                } else {
                    out.push_str(if negated { "[^" } else { "[" });
                    out.push_str(class);
                    out.push(']');
                }
            } else if matches!(e, 'b' | 'B') && !in_set {
                out.push_str(&format!("(?-u:\\{e})"));
            } else {
                out.push('\\');
                out.push(e);
            }
            continue;
        }
        if in_set && c == '-' && chars.get(i) == Some(&'-') {
            return None;
        }
        if in_set && matches!(c, '[' | '&' | '~') {
            out.push('\\');
        } else if c == '[' && !in_set {
            in_set = true;
        } else if c == ']' && in_set {
            in_set = false;
        }
        out.push(c);
    }
    Some(out)
}
