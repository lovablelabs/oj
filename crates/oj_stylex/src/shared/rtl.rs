//! Logical→physical declaration rewrites and the legacy RTL value flips.
// parity: babel-plugin src/shared/physical-rtl/{generate-ltr,generate-rtl}.js

use crate::options::{ResolvedOptions, StyleResolution};
use std::borrow::Cow;

use crate::shared::css_value::{Kind, Node, parse, stringify, unit};

/// The three options `generateLtr`/`generateRtl` read; upstream passes none
/// from `positionTry` or the keyframes stable string, so those take DEFAULTS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtlContext {
    pub style_resolution: StyleResolution,
    pub enable_logical_styles_polyfill: bool,
    pub enable_legacy_value_flipping: bool,
}

impl RtlContext {
    pub const DEFAULTS: RtlContext = RtlContext {
        style_resolution: StyleResolution::PropertySpecificity,
        enable_logical_styles_polyfill: false,
        enable_legacy_value_flipping: false,
    };

    pub fn of(options: &ResolvedOptions) -> RtlContext {
        RtlContext {
            style_resolution: options.style_resolution,
            enable_logical_styles_polyfill: options.enable_logical_styles_polyfill,
            enable_legacy_value_flipping: options.enable_legacy_value_flipping,
        }
    }

    fn legacy(&self) -> bool {
        self.style_resolution == StyleResolution::LegacyExpandShorthands
    }
}

fn ltr_value(value: &str) -> Option<&'static str> {
    match value {
        "start" | "inline-start" => Some("left"),
        "end" | "inline-end" => Some("right"),
        _ => None,
    }
}

fn rtl_value(value: &str) -> Option<&'static str> {
    match value {
        "start" | "inline-start" => Some("right"),
        "end" | "inline-end" => Some("left"),
        _ => None,
    }
}

fn ltr_property(key: &str) -> Option<&'static str> {
    Some(match key {
        "margin-start" => "margin-left",
        "margin-end" => "margin-right",
        "padding-start" => "padding-left",
        "padding-end" => "padding-right",
        "border-start" => "border-left",
        "border-end" => "border-right",
        "border-start-width" => "border-left-width",
        "border-end-width" => "border-right-width",
        "border-start-color" => "border-left-color",
        "border-end-color" => "border-right-color",
        "border-start-style" => "border-left-style",
        "border-end-style" => "border-right-style",
        "border-top-start-radius" => "border-top-left-radius",
        "border-bottom-start-radius" => "border-bottom-left-radius",
        "border-top-end-radius" => "border-top-right-radius",
        "border-bottom-end-radius" => "border-bottom-right-radius",
        "start" => "left",
        "end" => "right",
        _ => return None,
    })
}

fn rtl_property(key: &str) -> Option<&'static str> {
    Some(match key {
        "margin-start" => "margin-right",
        "margin-end" => "margin-left",
        "padding-start" => "padding-right",
        "padding-end" => "padding-left",
        "border-start" => "border-right",
        "border-end" => "border-left",
        "border-start-width" => "border-right-width",
        "border-end-width" => "border-left-width",
        "border-start-color" => "border-right-color",
        "border-end-color" => "border-left-color",
        "border-start-style" => "border-right-style",
        "border-end-style" => "border-left-style",
        "border-top-start-radius" => "border-top-right-radius",
        "border-bottom-start-radius" => "border-bottom-right-radius",
        "border-top-end-radius" => "border-top-left-radius",
        "border-bottom-end-radius" => "border-bottom-left-radius",
        "start" => "right",
        "end" => "left",
        _ => return None,
    })
}

// parity: generate-ltr.js inlinePropertyToLTR — consulted only under
// legacy-expand-shorthands with the polyfill on.
fn inline_ltr_property(key: &str) -> Option<&'static str> {
    Some(match key {
        "margin-inline-start" => "margin-left",
        "margin-inline-end" => "margin-right",
        "padding-inline-start" => "padding-left",
        "padding-inline-end" => "padding-right",
        "border-inline-start" => "border-left",
        "border-inline-end" => "border-right",
        "border-inline-start-width" => "border-left-width",
        "border-inline-end-width" => "border-right-width",
        "border-inline-start-color" => "border-left-color",
        "border-inline-end-color" => "border-right-color",
        "border-inline-start-style" => "border-left-style",
        "border-inline-end-style" => "border-right-style",
        "border-start-start-radius" => "border-top-left-radius",
        "border-start-end-radius" => "border-top-right-radius",
        "border-end-start-radius" => "border-bottom-left-radius",
        "border-end-end-radius" => "border-bottom-right-radius",
        "inset-inline-start" => "left",
        "inset-inline-end" => "right",
        _ => return None,
    })
}

fn inline_rtl_property(key: &str) -> Option<&'static str> {
    Some(match key {
        "margin-inline-start" => "margin-right",
        "margin-inline-end" => "margin-left",
        "padding-inline-start" => "padding-right",
        "padding-inline-end" => "padding-left",
        "border-inline-start" => "border-right",
        "border-inline-end" => "border-left",
        "border-inline-start-width" => "border-right-width",
        "border-inline-end-width" => "border-left-width",
        "border-inline-start-color" => "border-right-color",
        "border-inline-end-color" => "border-left-color",
        "border-inline-start-style" => "border-right-style",
        "border-inline-end-style" => "border-left-style",
        "border-start-start-radius" => "border-top-right-radius",
        "border-start-end-radius" => "border-top-left-radius",
        "border-end-start-radius" => "border-bottom-right-radius",
        "border-end-end-radius" => "border-bottom-left-radius",
        "inset-inline-start" => "right",
        "inset-inline-end" => "left",
        _ => return None,
    })
}

fn map_background_position(value: &str, start_to: &str, end_to: &str) -> String {
    value
        .split(' ')
        .map(|word| match word {
            "start" | "insetInlineStart" => start_to,
            "end" | "insetInlineEnd" => end_to,
            other => other,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The ltr declaration for a `[dashed-key, value]` pair (identity when no
/// rewrite applies).
pub fn generate_ltr<'k, 'v>(
    key: &'k str,
    value: &'v str,
    ctx: RtlContext,
) -> (Cow<'k, str>, Cow<'v, str>) {
    let identity = (Cow::Borrowed(key), Cow::Borrowed(value));
    if ctx.legacy() {
        if !ctx.enable_logical_styles_polyfill {
            return identity;
        }
        if let Some(mapped) = inline_ltr_property(key) {
            return (Cow::Borrowed(mapped), Cow::Borrowed(value));
        }
    }
    if let Some(mapped) = ltr_property(key) {
        return (Cow::Borrowed(mapped), Cow::Borrowed(value));
    }
    match key {
        "float" | "clear" => (
            Cow::Borrowed(key),
            Cow::Borrowed(ltr_value(value).unwrap_or(value)),
        ),
        "background-position" => (
            Cow::Borrowed(key),
            Cow::Owned(map_background_position(value, "left", "right")),
        ),
        _ => identity,
    }
}

/// The rtl declaration, or `None` when the pair does not flip.
pub fn generate_rtl<'k, 'v>(
    key: &'k str,
    value: &'v str,
    ctx: RtlContext,
) -> Option<(Cow<'k, str>, Cow<'v, str>)> {
    if ctx.legacy() {
        // The polyfill gates every rtl rule, legacy value flipping included.
        if !ctx.enable_logical_styles_polyfill {
            return None;
        }
        if let Some(mapped) = inline_rtl_property(key) {
            return Some((Cow::Borrowed(mapped), Cow::Borrowed(value)));
        }
    }
    if let Some(mapped) = rtl_property(key) {
        return Some((Cow::Borrowed(mapped), Cow::Borrowed(value)));
    }
    match key {
        "float" | "clear" => rtl_value(value).map(|v| (Cow::Borrowed(key), Cow::Borrowed(v))),
        "background-position" => {
            if !value.split(' ').any(|w| w == "start" || w == "end") {
                return None;
            }
            Some((
                Cow::Borrowed(key),
                Cow::Owned(map_background_position(value, "right", "left")),
            ))
        }
        "cursor" if ctx.enable_legacy_value_flipping => {
            cursor_flip(value).map(|v| (Cow::Borrowed(key), Cow::Borrowed(v)))
        }
        "box-shadow" | "text-shadow" if ctx.enable_legacy_value_flipping => {
            flip_shadow(value).map(|v| (Cow::Borrowed(key), Cow::Owned(v)))
        }
        _ => None,
    }
}

// parity: generate-rtl.js CURSOR_FLIP — whole-string, case-sensitive.
fn cursor_flip(value: &str) -> Option<&'static str> {
    Some(match value {
        "e-resize" => "w-resize",
        "w-resize" => "e-resize",
        "ne-resize" => "nw-resize",
        "nw-resize" => "ne-resize",
        "se-resize" => "sw-resize",
        "sw-resize" => "se-resize",
        "nesw-resize" => "nwse-resize",
        "nwse-resize" => "nesw-resize",
        _ => return None,
    })
}

/// Top-level `,` `/` `:` groups; empty groups drop and the join is always `,`.
fn split_by_divisor(value: &str) -> Vec<String> {
    let mut groups: Vec<String> = Vec::new();
    let mut current: Vec<Node> = Vec::new();
    for node in parse(value) {
        if node.kind == Kind::Div {
            if !current.is_empty() {
                groups.push(stringify(&current));
                current.clear();
            }
        } else {
            current.push(node);
        }
    }
    if !current.is_empty() {
        groups.push(stringify(&current));
    }
    groups
}

// parity: generate-rtl.js flipShadow — naive space split, string-only surgery.
fn flip_shadow(value: &str) -> Option<String> {
    let flipped: Vec<String> = split_by_divisor(value)
        .into_iter()
        .map(|def| {
            let mut parts: Vec<String> = def.split(' ').map(str::to_string).collect();
            let index = usize::from(!starts_like_number(&parts[0]));
            if index < parts.len() {
                parts[index] = flip_sign(&parts[index]);
            }
            parts.join(" ")
        })
        .collect();
    let rtl = flipped.join(",");
    (rtl != value).then_some(rtl)
}

fn starts_like_number(part: &str) -> bool {
    unit(part).is_some()
}

fn flip_sign(part: &str) -> String {
    if part == "0" {
        return part.to_string();
    }
    match part.strip_prefix('-') {
        Some(rest) => rest.to_string(),
        None => format!("-{part}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ltr(key: &str, value: &str, ctx: RtlContext) -> (String, String) {
        let (k, v) = generate_ltr(key, value, ctx);
        (k.into_owned(), v.into_owned())
    }

    fn rtl(key: &str, value: &str, ctx: RtlContext) -> Option<(String, String)> {
        generate_rtl(key, value, ctx).map(|(k, v)| (k.into_owned(), v.into_owned()))
    }

    fn legacy(polyfill: bool) -> RtlContext {
        RtlContext {
            style_resolution: StyleResolution::LegacyExpandShorthands,
            enable_logical_styles_polyfill: polyfill,
            enable_legacy_value_flipping: false,
        }
    }

    fn flipping() -> RtlContext {
        RtlContext {
            enable_legacy_value_flipping: true,
            ..RtlContext::DEFAULTS
        }
    }

    #[test]
    fn oracle_pinned_pairs() {
        // Pinned via probes 2026-08-27 against @stylexjs/babel-plugin 0.19.0.
        let d = RtlContext::DEFAULTS;
        assert_eq!(
            ltr("margin-start", "10px", d),
            ("margin-left".to_string(), "10px".to_string())
        );
        assert_eq!(
            rtl("margin-start", "10px", d),
            Some(("margin-right".to_string(), "10px".to_string()))
        );
        assert_eq!(
            ltr("float", "inline-start", d),
            ("float".to_string(), "left".to_string())
        );
        assert_eq!(
            rtl("float", "inline-start", d),
            Some(("float".to_string(), "right".to_string()))
        );
        assert_eq!(
            ltr("float", "left", d),
            ("float".to_string(), "left".to_string())
        );
        assert_eq!(rtl("float", "left", d), None);
        assert_eq!(
            ltr("background-position", "start top", d),
            ("background-position".to_string(), "left top".to_string())
        );
        assert_eq!(
            rtl("background-position", "start top", d),
            Some(("background-position".to_string(), "right top".to_string()))
        );
        assert_eq!(rtl("background-position", "left top", d), None);
        assert_eq!(rtl("cursor", "e-resize", d), None);
        assert_eq!(rtl("margin-inline-start", "1px", d), None);
        assert_eq!(
            ltr("margin-inline-start", "1px", d),
            ("margin-inline-start".to_string(), "1px".to_string())
        );
    }

    #[test]
    fn legacy_polyfill_gates_every_rtl_rule() {
        // polyfill off: ltr is identity for everything and there is no rtl.
        assert_eq!(
            ltr("margin-inline-start", "1px", legacy(false)),
            ("margin-inline-start".to_string(), "1px".to_string())
        );
        assert_eq!(rtl("margin-inline-start", "1px", legacy(false)), None);
        assert_eq!(
            ltr("margin-start", "1px", legacy(false)),
            ("margin-start".to_string(), "1px".to_string())
        );
        assert_eq!(rtl("margin-start", "1px", legacy(false)), None);
        assert_eq!(
            ltr("background-position", "start top", legacy(false)),
            ("background-position".to_string(), "start top".to_string())
        );

        // polyfill on: the inline table fires first, then the normal one.
        assert_eq!(
            ltr("border-start-start-radius", "1px", legacy(true)),
            ("border-top-left-radius".to_string(), "1px".to_string())
        );
        assert_eq!(
            rtl("border-start-start-radius", "1px", legacy(true)),
            Some(("border-top-right-radius".to_string(), "1px".to_string()))
        );
        assert_eq!(
            ltr("border-start-width", "1px", legacy(true)),
            ("border-left-width".to_string(), "1px".to_string())
        );
        // camelCase background-position words map in ltr but never produce rtl.
        assert_eq!(
            ltr("background-position", "insetInlineStart 0", legacy(true)),
            ("background-position".to_string(), "left 0".to_string())
        );
        assert_eq!(
            rtl("background-position", "insetInlineStart 0", legacy(true)),
            None
        );
    }

    #[test]
    fn legacy_value_flipping_is_gated_by_the_polyfill() {
        let leg_flip = RtlContext {
            enable_legacy_value_flipping: true,
            ..legacy(false)
        };
        assert_eq!(rtl("cursor", "e-resize", leg_flip), None);
        let leg_flip_poly = RtlContext {
            enable_logical_styles_polyfill: true,
            ..leg_flip
        };
        assert_eq!(
            rtl("cursor", "e-resize", leg_flip_poly),
            Some(("cursor".to_string(), "w-resize".to_string()))
        );
    }

    #[test]
    fn cursor_and_shadow_flips() {
        let f = flipping();
        let rtl = |k: &str, v: &str| rtl(k, v, f).map(|(_, v)| v);
        assert_eq!(rtl("cursor", "nesw-resize").as_deref(), Some("nwse-resize"));
        assert_eq!(rtl("cursor", "n-resize"), None);
        assert_eq!(rtl("cursor", "E-resize"), None);
        assert_eq!(rtl("cursor", "url(a.png),e-resize"), None);
        assert_eq!(ltr("cursor", "e-resize", f).1, "e-resize");

        assert_eq!(
            rtl("box-shadow", "1px 1px #000").as_deref(),
            Some("-1px 1px #000")
        );
        assert_eq!(
            rtl("text-shadow", "1px 1px #000").as_deref(),
            Some("-1px 1px #000")
        );
        assert_eq!(
            rtl("box-shadow", "-1px -1px #000").as_deref(),
            Some("1px -1px #000")
        );
        assert_eq!(
            rtl("box-shadow", "inset 1px 1px #000").as_deref(),
            Some("inset -1px 1px #000")
        );
        assert_eq!(rtl("box-shadow", "0 1px 2px #000"), None);
        assert_eq!(
            rtl("box-shadow", "0px 1px 2px #000").as_deref(),
            Some("-0px 1px 2px #000")
        );
        assert_eq!(rtl("box-shadow", "none"), None);
        assert_eq!(rtl("box-shadow", "var(--shadow)"), None);
        // `/` and `:` are divisors too, and the join is always a comma.
        assert_eq!(
            rtl("box-shadow", "1px 1px / 2px 2px").as_deref(),
            Some("-1px 1px,-2px 2px")
        );
        assert_eq!(rtl("box-shadow", "1px : 2px").as_deref(), Some("-1px,-2px"));
        assert_eq!(
            rtl("box-shadow", "1px 1px red,,").as_deref(),
            Some("-1px 1px red")
        );
        assert_eq!(
            rtl("box-shadow", "1px 1px #000,,2px 2px red").as_deref(),
            Some("-1px 1px #000,-2px 2px red")
        );
        // Commas and slashes nested in a function are not divisors.
        assert_eq!(
            rtl("box-shadow", "1px 1px hsl(0 0% 0% / 50%)").as_deref(),
            Some("-1px 1px hsl(0 0% 0% / 50%)")
        );
        assert_eq!(
            rtl("box-shadow", "min(1px,2px) 1px red").as_deref(),
            Some("min(1px,2px) -1px red")
        );
        // Upstream corrupts these; reproduce it.
        assert_eq!(
            rtl("box-shadow", "var(--a,1px 1px red)").as_deref(),
            Some("var(--a,1px -1px red)")
        );
        assert_eq!(
            rtl("box-shadow", "calc(1px + 2px) 1px red").as_deref(),
            Some("calc(1px -+ 2px) 1px red")
        );
        assert_eq!(rtl("box-shadow", ".px 1px").as_deref(), Some(".px -1px"));
        assert_eq!(
            rtl("box-shadow", "- 1px 1px").as_deref(),
            Some("- -1px 1px")
        );
        assert_eq!(rtl("box-shadow", "+1px 1px").as_deref(), Some("-+1px 1px"));
        assert_eq!(rtl("box-shadow", ".5px 1px").as_deref(), Some("-.5px 1px"));
        assert_eq!(
            rtl("box-shadow", "1px 1px red /* c */").as_deref(),
            Some("-1px 1px red /* c */")
        );
        assert_eq!(
            rtl("box-shadow", "none,1px 1px red").as_deref(),
            Some("none,-1px 1px red")
        );
    }
}
