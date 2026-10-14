//! Vite's `css.lightningcss` as its build minify applies it (css.ts
//! minifyCSS): the options spread into lightningcss's transform, with
//! `targets` replaced by `build.cssTarget` and `cssModules`, `visitor` and
//! `customAtRules` dropped. What is left: `include`/`exclude` (feature bits,
//! the same numbers as the `Features` the npm package exports), `drafts`,
//! `nonStandard`, `pseudoClasses` and `unusedSymbols`.

use std::collections::HashSet;

use lightningcss::printer::PseudoClasses;
use lightningcss::stylesheet::ParserFlags;
use lightningcss::targets::{Features, Targets};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LightningOptions {
    pub include: u32,
    pub exclude: u32,
    /// `drafts.customMedia`.
    pub custom_media: bool,
    /// `nonStandard.deepSelectorCombinator`.
    pub deep_selector_combinator: bool,
    /// `pseudoClasses`: hover, active, focus, focusVisible, focusWithin.
    pub pseudo_classes: [Option<String>; 5],
    pub unused_symbols: Vec<String>,
}

impl LightningOptions {
    /// From the `css.lightningcss` object; anything malformed is ignored.
    pub fn from_json(v: &serde_json::Value) -> LightningOptions {
        let flag = |obj: &str, key: &str| {
            v.get(obj)
                .and_then(|o| o.get(key))
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
        };
        let bits = |key: &str| v.get(key).and_then(|n| n.as_u64()).unwrap_or(0) as u32;
        let pseudo = |key: &str| {
            v.get("pseudoClasses")
                .and_then(|p| p.get(key))
                .and_then(|s| s.as_str())
                .map(str::to_string)
        };
        LightningOptions {
            include: bits("include"),
            exclude: bits("exclude"),
            custom_media: flag("drafts", "customMedia"),
            deep_selector_combinator: flag("nonStandard", "deepSelectorCombinator"),
            pseudo_classes: ["hover", "active", "focus", "focusVisible", "focusWithin"].map(pseudo),
            unused_symbols: v
                .get("unusedSymbols")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    pub(crate) fn parser_flags(&self) -> ParserFlags {
        let mut flags = ParserFlags::empty();
        flags.set(ParserFlags::CUSTOM_MEDIA, self.custom_media);
        flags.set(
            ParserFlags::DEEP_SELECTOR_COMBINATOR,
            self.deep_selector_combinator,
        );
        flags
    }

    pub(crate) fn targets(&self, mut targets: Targets) -> Targets {
        targets.include = Features::from_bits_truncate(self.include);
        targets.exclude = Features::from_bits_truncate(self.exclude);
        targets
    }

    pub(crate) fn pseudo_classes(&self) -> Option<PseudoClasses<'_>> {
        let [hover, active, focus, focus_visible, focus_within] =
            self.pseudo_classes.each_ref().map(|p| p.as_deref());
        self.pseudo_classes
            .iter()
            .any(Option::is_some)
            .then_some(PseudoClasses {
                hover,
                active,
                focus,
                focus_visible,
                focus_within,
            })
    }

    pub(crate) fn unused_symbols(&self) -> HashSet<String> {
        self.unused_symbols.iter().cloned().collect()
    }
}
