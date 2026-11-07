//! Property/pseudo/at-rule priority tables.
// parity: @stylexjs/shared src/utils/property-priorities.js

/// Priority for one selector-path segment: a property key, a pseudo, an
/// at-rule, or a `when.*` relational `:where(...)` selector.
pub fn get_priority(key: &str) -> f64 {
    if let Some(p) = at_rule_priority(key) {
        return p;
    }
    if let Some(p) = compound_pseudo_priority(key) {
        return p;
    }
    if let Some(p) = pseudo_element_priority(key) {
        return p;
    }
    if let Some(p) = pseudo_class_priority(key) {
        return p;
    }
    default_priority(key).unwrap_or(3000.0)
}

fn at_rule_priority(key: &str) -> Option<f64> {
    if key.starts_with("--") {
        return Some(1.0);
    }
    if key.starts_with("@supports") {
        return Some(30.0);
    }
    if key.starts_with("@media") {
        return Some(200.0);
    }
    if key.starts_with("@container") {
        return Some(300.0);
    }
    None
}

fn pseudo_element_priority(key: &str) -> Option<f64> {
    key.starts_with("::").then_some(5000.0)
}

fn pseudo_class_table(pseudo: &str) -> Option<f64> {
    match pseudo {
        ":is" => Some(40.0),
        ":where" => Some(40.0),
        ":not" => Some(40.0),
        ":has" => Some(45.0),
        ":dir" => Some(50.0),
        ":lang" => Some(51.0),
        ":first-child" => Some(52.0),
        ":first-of-type" => Some(53.0),
        ":last-child" => Some(54.0),
        ":last-of-type" => Some(55.0),
        ":only-child" => Some(56.0),
        ":only-of-type" => Some(57.0),
        ":nth-child" => Some(60.0),
        ":nth-last-child" => Some(61.0),
        ":nth-of-type" => Some(62.0),
        ":nth-last-of-type" => Some(63.0),
        ":empty" => Some(70.0),
        ":link" => Some(80.0),
        ":any-link" => Some(81.0),
        ":local-link" => Some(82.0),
        ":target-within" => Some(83.0),
        ":target" => Some(84.0),
        ":visited" => Some(85.0),
        ":enabled" => Some(91.0),
        ":disabled" => Some(92.0),
        ":required" => Some(93.0),
        ":optional" => Some(94.0),
        ":read-only" => Some(95.0),
        ":read-write" => Some(96.0),
        ":placeholder-shown" => Some(97.0),
        ":in-range" => Some(98.0),
        ":out-of-range" => Some(99.0),
        ":default" => Some(100.0),
        ":checked" => Some(101.0),
        ":indeterminate" => Some(101.0),
        ":blank" => Some(102.0),
        ":valid" => Some(103.0),
        ":invalid" => Some(104.0),
        ":user-invalid" => Some(105.0),
        ":autofill" => Some(110.0),
        ":picture-in-picture" => Some(120.0),
        ":modal" => Some(121.0),
        ":fullscreen" => Some(122.0),
        ":paused" => Some(123.0),
        ":playing" => Some(124.0),
        ":current" => Some(125.0),
        ":past" => Some(126.0),
        ":future" => Some(127.0),
        ":hover" => Some(130.0),
        ":focusWithin" => Some(140.0),
        ":focus" => Some(150.0),
        ":focusVisible" => Some(160.0),
        ":active" => Some(170.0),
        _ => None,
    }
}

// Chains of simple pseudo-classes/elements only; any functional part opts out.
fn compound_pseudo_priority(key: &str) -> Option<f64> {
    let parts = pseudo_parts(key);
    if parts.len() <= 1 || parts.iter().any(|p| p.contains('(')) {
        return None;
    }
    let mut total = 0.0;
    for part in parts {
        total += if part.starts_with("::") {
            5000.0
        } else {
            pseudo_class_table(part).unwrap_or(40.0)
        };
    }
    Some(total)
}

// Equivalent of matching /::[a-zA-Z-]+|:[a-zA-Z-]+(?:\([^)]*\))?/g.
fn pseudo_parts(key: &str) -> Vec<&str> {
    let bytes = key.as_bytes();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b':'
            && let Some(end) = pseudo_part_end(bytes, i)
        {
            parts.push(&key[i..end]);
            i = end;
            continue;
        }
        i += 1;
    }
    parts
}

fn pseudo_part_end(bytes: &[u8], start: usize) -> Option<usize> {
    let is_name = |b: u8| b.is_ascii_alphabetic() || b == b'-';
    if bytes.get(start + 1) == Some(&b':') {
        let mut j = start + 2;
        while bytes.get(j).is_some_and(|&b| is_name(b)) {
            j += 1;
        }
        return (j > start + 2).then_some(j);
    }
    let mut j = start + 1;
    while bytes.get(j).is_some_and(|&b| is_name(b)) {
        j += 1;
    }
    if j == start + 1 {
        return None;
    }
    if bytes.get(j) == Some(&b'(')
        && let Some(rel) = bytes[j + 1..].iter().position(|&b| b == b')')
    {
        return Some(j + 1 + rel + 1);
    }
    Some(j)
}

fn pseudo_class_priority(key: &str) -> Option<f64> {
    if !key.starts_with(':') {
        return None;
    }
    let base = |p: &str| pseudo_class_table(p).unwrap_or(40.0) / 100.0;

    // Every relational shape opens with this literal; plain keys and ordinary
    // pseudos skip the five scanner allocations (hot per style entry).
    if key.starts_with(":where(") {
        if let Some(p) = match_relational(key, Relational::Ancestor) {
            return Some(10.0 + base(p.0));
        }
        if let Some(p) = match_relational(key, Relational::Descendant) {
            return Some(15.0 + base(p.0));
        }
        if let Some(p) = match_relational(key, Relational::AnySibling) {
            return Some(20.0 + base(p.0).max(base(p.1.unwrap_or(p.0))));
        }
        if let Some(p) = match_relational(key, Relational::SiblingBefore) {
            return Some(30.0 + base(p.0));
        }
        if let Some(p) = match_relational(key, Relational::SiblingAfter) {
            return Some(40.0 + base(p.0));
        }
    }

    let prop = key.split('(').next().unwrap_or(key);
    Some(pseudo_class_table(prop).unwrap_or(40.0))
}

// The five `when.*` output shapes (property-priorities.js RELATIONAL_SELECTORS).
enum Relational {
    Ancestor,
    Descendant,
    SiblingBefore,
    SiblingAfter,
    AnySibling,
}

struct RelScanner<'a> {
    chars: Vec<char>,
    src: &'a str,
    pos: usize,
}

impl<'a> RelScanner<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            chars: src.chars().collect(),
            src,
            pos: 0,
        }
    }

    fn lit(&mut self, s: &str) -> bool {
        for expected in s.chars() {
            if self.chars.get(self.pos) != Some(&expected) {
                return false;
            }
            self.pos += 1;
        }
        true
    }

    // \.[0-9a-zA-Z_-]+ — the marker class; ≥1 char
    fn class_name(&mut self) -> bool {
        if !self.lit(".") {
            return false;
        }
        let start = self.pos;
        while self
            .chars
            .get(self.pos)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        {
            self.pos += 1;
        }
        self.pos > start
    }

    // (:[a-zA-Z-]+) — the captured pseudo; ≥1 name char
    fn pseudo_capture(&mut self) -> Option<&'a str> {
        if self.chars.get(self.pos) != Some(&':') {
            return None;
        }
        let start = self.pos;
        self.pos += 1;
        while self
            .chars
            .get(self.pos)
            .is_some_and(|c| c.is_ascii_alphabetic() || *c == '-')
        {
            self.pos += 1;
        }
        if self.pos == start + 1 {
            return None;
        }
        let byte_start: usize = self.chars[..start].iter().map(|c| c.len_utf8()).sum();
        let byte_len: usize = self.chars[start..self.pos]
            .iter()
            .map(|c| c.len_utf8())
            .sum();
        Some(&self.src[byte_start..byte_start + byte_len])
    }

    fn ws(&mut self, at_least_one: bool, at_most_one: bool) -> bool {
        let start = self.pos;
        while self.chars.get(self.pos).is_some_and(|c| is_js_ws(*c)) {
            self.pos += 1;
            if at_most_one {
                break;
            }
        }
        !at_least_one || self.pos > start
    }

    fn eof(&self) -> bool {
        self.pos == self.chars.len()
    }
}

// JS /\s/: ECMA-262 WhiteSpace + LineTerminator set.
fn is_js_ws(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | ' ' | '\u{00A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

fn match_relational(key: &str, shape: Relational) -> Option<(&str, Option<&str>)> {
    let mut s = RelScanner::new(key);
    if !s.lit(":where(") {
        return None;
    }
    match shape {
        Relational::Ancestor => {
            // :where(\.CLASS(:pseudo)\s+\*)
            if !s.class_name() {
                return None;
            }
            let p = s.pseudo_capture()?;
            (s.ws(true, false) && s.lit("*)") && s.eof()).then_some((p, None))
        }
        Relational::Descendant => {
            // :where(:has(\.CLASS(:pseudo)))
            if !s.lit(":has(") || !s.class_name() {
                return None;
            }
            let p = s.pseudo_capture()?;
            (s.lit("))") && s.eof()).then_some((p, None))
        }
        Relational::SiblingBefore => {
            // :where(\.CLASS(:pseudo)\s+~\s+\*)
            if !s.class_name() {
                return None;
            }
            let p = s.pseudo_capture()?;
            (s.ws(true, false) && s.lit("~") && s.ws(true, false) && s.lit("*)") && s.eof())
                .then_some((p, None))
        }
        Relational::SiblingAfter => {
            // :where(:has(~\s\.CLASS(:pseudo)))
            if !s.lit(":has(~") || !s.ws(true, true) || !s.class_name() {
                return None;
            }
            let p = s.pseudo_capture()?;
            (s.lit("))") && s.eof()).then_some((p, None))
        }
        Relational::AnySibling => {
            // :where(\.CLASS(:p1)\s+~\s+\*,\s+:has(~\s\.CLASS(:p2)))
            if !s.class_name() {
                return None;
            }
            let p1 = s.pseudo_capture()?;
            if !(s.ws(true, false) && s.lit("~") && s.ws(true, false) && s.lit("*,")) {
                return None;
            }
            if !s.ws(true, false) || !s.lit(":has(~") || !s.ws(true, true) || !s.class_name() {
                return None;
            }
            let p2 = s.pseudo_capture()?;
            (s.lit("))") && s.eof()).then_some((p1, Some(p2)))
        }
    }
}

// Tier tables transcribed from property-priorities.js (MDN-derived data),
// including upstream's literal 'border-block-stylex' entry.
fn default_priority(key: &str) -> Option<f64> {
    let tier = match key {
        "animation" | "background" | "border" | "border-block" | "border-inline" | "margin"
        | "padding" | "font" | "grid" | "grid-template" | "grid-area" | "all" | "inset"
        | "scroll-margin" | "scroll-padding" => 1000.0,
        "animation-range"
        | "scroll-timeline"
        | "view-timeline"
        | "background-position"
        | "border-color"
        | "border-style"
        | "border-width"
        | "border-block-start"
        | "border-top"
        | "border-block-end"
        | "border-bottom"
        | "border-inline-color"
        | "border-inline-style"
        | "border-inline-width"
        | "border-inline-start"
        | "border-left"
        | "border-inline-end"
        | "border-right"
        | "border-image"
        | "border-radius"
        | "corner-shape"
        | "caret"
        | "outline"
        | "grid-gap"
        | "gap"
        | "place-content"
        | "place-items"
        | "place-self"
        | "margin-block"
        | "margin-inline"
        | "overscroll-behavior"
        | "padding-block"
        | "padding-inline"
        | "columns"
        | "column-rule"
        | "contain-intrinsic-size"
        | "container"
        | "flex"
        | "flex-flow"
        | "font-variant"
        | "grid-template-areas"
        | "grid-row"
        | "grid-column"
        | "list-style"
        | "mask"
        | "mask-border"
        | "offset"
        | "overflow"
        | "inset-block"
        | "inset-inline"
        | "scroll-margin-block"
        | "scroll-margin-inline"
        | "scroll-padding-block"
        | "scroll-padding-inline"
        | "scroll-snap-type"
        | "text-decoration"
        | "text-emphasis"
        | "transition" => 2000.0,
        "background-blend-mode"
        | "isolation"
        | "mix-blend-mode"
        | "animation-composition"
        | "animation-delay"
        | "animation-direction"
        | "animation-duration"
        | "animation-fill-mode"
        | "animation-iteration-count"
        | "animation-name"
        | "animation-play-state"
        | "animation-range-end"
        | "animation-range-start"
        | "animation-timing-function"
        | "animation-timeline"
        | "scroll-timeline-axis"
        | "scroll-timeline-name"
        | "timeline-scope"
        | "view-timeline-axis"
        | "view-timeline-inset"
        | "view-timeline-name"
        | "background-attachment"
        | "background-clip"
        | "background-color"
        | "background-image"
        | "background-origin"
        | "background-repeat"
        | "background-size"
        | "background-position-x"
        | "background-position-y"
        | "border-block-color"
        | "border-block-stylex"
        | "border-block-width"
        | "border-block-start-color"
        | "border-block-start-style"
        | "border-block-start-width"
        | "border-block-end-color"
        | "border-block-end-style"
        | "border-block-end-width"
        | "border-inline-start-color"
        | "border-inline-start-style"
        | "border-inline-start-width"
        | "border-inline-end-color"
        | "border-inline-end-style"
        | "border-inline-end-width"
        | "border-image-outset"
        | "border-image-repeat"
        | "border-image-slice"
        | "border-image-source"
        | "border-image-width"
        | "border-start-end-radius"
        | "border-start-start-radius"
        | "border-end-end-radius"
        | "border-end-start-radius"
        | "corner-start-start-shape"
        | "corner-start-end-shape"
        | "corner-end-start-shape"
        | "corner-end-end-shape"
        | "box-shadow"
        | "accent-color"
        | "appearance"
        | "aspect-ratio"
        | "caret-color"
        | "caret-shape"
        | "cursor"
        | "ime-mode"
        | "input-security"
        | "outline-color"
        | "outline-offset"
        | "outline-style"
        | "outline-width"
        | "pointer-events"
        | "resize"
        | "text-overflow"
        | "user-select"
        | "grid-row-gap"
        | "row-gap"
        | "grid-column-gap"
        | "column-gap"
        | "align-content"
        | "justify-content"
        | "align-items"
        | "justify-items"
        | "align-self"
        | "justify-self"
        | "box-sizing"
        | "block-size"
        | "inline-size"
        | "max-block-size"
        | "max-inline-size"
        | "min-block-size"
        | "min-inline-size"
        | "margin-block-start"
        | "margin-block-end"
        | "margin-inline-start"
        | "margin-inline-end"
        | "margin-trim"
        | "overscroll-behavior-block"
        | "overscroll-behavior-inline"
        | "padding-block-start"
        | "padding-block-end"
        | "padding-inline-start"
        | "padding-inline-end"
        | "visibility"
        | "color"
        | "color-scheme"
        | "forced-color-adjust"
        | "opacity"
        | "print-color-adjust"
        | "column-count"
        | "column-width"
        | "column-fill"
        | "column-span"
        | "column-rule-color"
        | "column-rule-style"
        | "column-rule-width"
        | "contain"
        | "contain-intrinsic-block-size"
        | "contain-intrinsic-width"
        | "contain-intrinsic-height"
        | "contain-intrinsic-inline-size"
        | "container-name"
        | "container-type"
        | "content-visibility"
        | "counter-increment"
        | "counter-reset"
        | "counter-set"
        | "display"
        | "flex-basis"
        | "flex-grow"
        | "flex-shrink"
        | "flex-direction"
        | "flex-wrap"
        | "order"
        | "font-family"
        | "font-size"
        | "font-stretch"
        | "font-style"
        | "font-weight"
        | "line-height"
        | "font-variant-alternates"
        | "font-variant-caps"
        | "font-variant-east-asian"
        | "font-variant-emoji"
        | "font-variant-ligatures"
        | "font-variant-numeric"
        | "font-variant-position"
        | "font-feature-settings"
        | "font-kerning"
        | "font-language-override"
        | "font-optical-sizing"
        | "font-palette"
        | "font-variation-settings"
        | "font-size-adjust"
        | "font-smooth"
        | "font-synthesis-position"
        | "font-synthesis-small-caps"
        | "font-synthesis-style"
        | "font-synthesis-weight"
        | "line-height-step"
        | "box-decoration-break"
        | "break-after"
        | "break-before"
        | "break-inside"
        | "orphans"
        | "widows"
        | "content"
        | "quotes"
        | "grid-auto-flow"
        | "grid-auto-rows"
        | "grid-auto-columns"
        | "grid-template-columns"
        | "grid-template-rows"
        | "grid-row-start"
        | "grid-row-end"
        | "grid-column-start"
        | "grid-column-end"
        | "align-tracks"
        | "justify-tracks"
        | "masonry-auto-flow"
        | "image-orientation"
        | "image-rendering"
        | "image-resolution"
        | "object-fit"
        | "object-position"
        | "initial-letter"
        | "initial-letter-align"
        | "list-style-image"
        | "list-style-position"
        | "list-style-type"
        | "clip"
        | "clip-path"
        | "mask-clip"
        | "mask-composite"
        | "mask-image"
        | "mask-mode"
        | "mask-origin"
        | "mask-position"
        | "mask-repeat"
        | "mask-size"
        | "mask-type"
        | "mask-border-mode"
        | "mask-border-outset"
        | "mask-border-repeat"
        | "mask-border-slice"
        | "mask-border-source"
        | "mask-border-width"
        | "text-rendering"
        | "offset-anchor"
        | "offset-distance"
        | "offset-path"
        | "offset-position"
        | "offset-rotate"
        | "-webkit-box-orient"
        | "-webkit-line-clamp"
        | "overflow-block"
        | "overflow-inline"
        | "overflow-clip-margin"
        | "scroll-gutter"
        | "scroll-behavior"
        | "page"
        | "page-break-after"
        | "page-break-before"
        | "page-break-inside"
        | "inset-block-start"
        | "inset-block-end"
        | "inset-inline-start"
        | "inset-inline-end"
        | "clear"
        | "float"
        | "position"
        | "z-index"
        | "ruby-align"
        | "ruby-merge"
        | "ruby-position"
        | "overflow-anchor"
        | "scroll-margin-block-start"
        | "scroll-margin-block-end"
        | "scroll-margin-inline-start"
        | "scroll-margin-inline-end"
        | "scroll-padding-block-start"
        | "scroll-padding-block-end"
        | "scroll-padding-inline-start"
        | "scroll-padding-inline-end"
        | "scroll-snap-align"
        | "scroll-snap-stop"
        | "scrollbar-color"
        | "scrollbar-width"
        | "shape-image-threshold"
        | "shape-margin"
        | "shape-outside"
        | "azimuth"
        | "border-collapse"
        | "border-spacing"
        | "caption-side"
        | "empty-cells"
        | "table-layout"
        | "vertical-align"
        | "text-decoration-color"
        | "text-decoration-line"
        | "text-decoration-skip"
        | "text-decoration-skip-ink"
        | "text-decoration-style"
        | "text-decoration-thickness"
        | "text-emphasis-color"
        | "text-emphasis-position"
        | "text-emphasis-style"
        | "text-shadow"
        | "text-underline-offset"
        | "text-underline-position"
        | "hanging-punctuation"
        | "hyphenate-character"
        | "hyphenate-limit-chars"
        | "hyphens"
        | "letter-spacing"
        | "line-break"
        | "overflow-wrap"
        | "paint-order"
        | "tab-size"
        | "text-align"
        | "text-align-last"
        | "text-indent"
        | "text-justify"
        | "text-size-adjust"
        | "text-transform"
        | "text-wrap"
        | "white-space"
        | "white-space-collapse"
        | "word-break"
        | "word-spacing"
        | "word-wrap"
        | "backface-visibility"
        | "perspective"
        | "perspective-origin"
        | "rotate"
        | "scale"
        | "transform"
        | "transform-box"
        | "transform-origin"
        | "transform-style"
        | "translate"
        | "transition-delay"
        | "transition-duration"
        | "transition-property"
        | "transition-timing-function"
        | "view-transition-name"
        | "will-change"
        | "direction"
        | "text-combine-upright"
        | "text-orientation"
        | "unicode-bidi"
        | "writing-mode"
        | "backdrop-filter"
        | "filter"
        | "math-depth"
        | "math-shift"
        | "math-style"
        | "touch-action" => 3000.0,
        "border-top-color"
        | "border-top-style"
        | "border-top-width"
        | "border-bottom-color"
        | "border-bottom-style"
        | "border-bottom-width"
        | "border-left-color"
        | "border-left-style"
        | "border-left-width"
        | "border-right-color"
        | "border-right-style"
        | "border-right-width"
        | "border-top-left-radius"
        | "border-top-right-radius"
        | "border-bottom-left-radius"
        | "border-bottom-right-radius"
        | "corner-top-left-shape"
        | "corner-top-right-shape"
        | "corner-bottom-left-shape"
        | "corner-bottom-right-shape"
        | "height"
        | "width"
        | "max-height"
        | "max-width"
        | "min-height"
        | "min-width"
        | "margin-top"
        | "margin-bottom"
        | "margin-left"
        | "margin-right"
        | "overscroll-behavior-y"
        | "overscroll-behavior-x"
        | "padding-top"
        | "padding-bottom"
        | "padding-left"
        | "padding-right"
        | "overflow-y"
        | "overflow-x"
        | "top"
        | "bottom"
        | "left"
        | "right"
        | "scroll-margin-top"
        | "scroll-margin-bottom"
        | "scroll-margin-left"
        | "scroll-margin-right"
        | "scroll-padding-top"
        | "scroll-padding-bottom"
        | "scroll-padding-left"
        | "scroll-padding-right" => 4000.0,
        _ => return None,
    };
    Some(tier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_values() {
        assert_eq!(get_priority("--anything"), 1.0);
        assert_eq!(get_priority("@supports (display: grid)"), 30.0);
        assert_eq!(get_priority("@media (min-width: 600px)"), 200.0);
        assert_eq!(get_priority("@container (min-width: 100px)"), 300.0);
        assert_eq!(get_priority(":hover"), 130.0);
        assert_eq!(get_priority(":focusWithin"), 140.0);
        assert_eq!(get_priority(":focus-within"), 40.0);
        assert_eq!(get_priority("::before"), 5000.0);
        assert_eq!(get_priority(":hover:active"), 300.0);
        assert_eq!(get_priority("::before:hover"), 5130.0);
        assert_eq!(get_priority(":nth-child(2n)"), 60.0);
        // functional part in a chain opts out of compound handling
        assert_eq!(get_priority(":hover:nth-child(2n)"), 40.0);
        assert_eq!(get_priority("[data-open]"), 3000.0);
        assert_eq!(get_priority("margin"), 1000.0);
        assert_eq!(get_priority("margin-inline"), 2000.0);
        assert_eq!(get_priority("margin-inline-start"), 3000.0);
        assert_eq!(get_priority("margin-top"), 4000.0);
        assert_eq!(get_priority("unknown-prop"), 3000.0);
    }

    #[test]
    fn relational_shapes() {
        let m = "x-default-marker";
        assert_eq!(get_priority(&format!(":where(.{m}:hover *)")), 11.3);
        assert_eq!(get_priority(&format!(":where(:has(.{m}:checked))")), 16.01);
        assert_eq!(get_priority(&format!(":where(.{m}:focus ~ *)")), 31.5);
        assert_eq!(get_priority(&format!(":where(:has(~ .{m}:active))")), 41.7);
        assert_eq!(
            get_priority(&format!(":where(.{m}:hover ~ *, :has(~ .{m}:hover))")),
            21.3
        );
        // attribute selectors do not match the relational shapes
        assert_eq!(get_priority(&format!(":where(.{m}[data-open] *)")), 40.0);
    }
}
