//! styleResolution dispatch: the three shorthand-expansion tables.
// parity: babel-plugin src/shared/preprocess-rules/*.js

use crate::errors::StylexError;
use crate::options::{PropertyValidationMode, ResolvedOptions, StyleResolution};
use crate::shared::flatten::StyleScalar;
use crate::shared::split_css_value::split_value;
use std::borrow::Cow;

/// One expanded declaration: the resolved property and its value, where `None`
/// is the `null` punch that stops an earlier longhand from leaking through.
pub type ExpandedPair<'a> = (Cow<'a, str>, Option<StyleScalar<'a>>);

/// The expansion result; the unexpanded (identity) case carries its pair inline.
#[derive(Debug)]
pub enum Expanded<'a> {
    One(ExpandedPair<'a>),
    Many(Vec<ExpandedPair<'a>>),
}

impl<'a> Expanded<'a> {
    pub fn as_slice(&self) -> &[ExpandedPair<'a>] {
        match self {
            Expanded::One(pair) => std::slice::from_ref(pair),
            Expanded::Many(pairs) => pairs,
        }
    }

    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    pub fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, ExpandedPair<'a>> {
        self.as_slice().iter()
    }
}

impl PartialEq for Expanded<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

pub enum ExpandedIter<'a> {
    One(Option<ExpandedPair<'a>>),
    Many(std::vec::IntoIter<ExpandedPair<'a>>),
}

impl<'a> Iterator for ExpandedIter<'a> {
    type Item = ExpandedPair<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            ExpandedIter::One(pair) => pair.take(),
            ExpandedIter::Many(pairs) => pairs.next(),
        }
    }
}

impl<'a> IntoIterator for Expanded<'a> {
    type Item = ExpandedPair<'a>;
    type IntoIter = ExpandedIter<'a>;

    fn into_iter(self) -> ExpandedIter<'a> {
        match self {
            Expanded::One(pair) => ExpandedIter::One(Some(pair)),
            Expanded::Many(pairs) => ExpandedIter::Many(pairs.into_iter()),
        }
    }
}

const LOGICAL_START_VAR: &str = "var(--stylex-logical-start)";
const LOGICAL_END_VAR: &str = "var(--stylex-logical-end)";

/// `flatMapExpandedShorthands`: an empty result is the silent drop.
pub fn flat_map_expanded_shorthands<'a>(
    key: Cow<'a, str>,
    value: Option<StyleScalar<'a>>,
    value_is_array: bool,
    options: &ResolvedOptions,
) -> Result<Expanded<'a>, StylexError> {
    let key: Cow<'a, str> = if key.starts_with("var(") && key.ends_with(')') {
        match key {
            Cow::Borrowed(k) => Cow::Borrowed(&k[4..k.len() - 1]),
            Cow::Owned(k) => Cow::Owned(k[4..k.len() - 1].to_string()),
        }
    } else {
        key
    };
    match options.style_resolution {
        StyleResolution::PropertySpecificity => {
            let expansion = property_specificity_expansion(&key);
            if matches!(expansion, Expansion::None) {
                return Ok(Expanded::One((key, value)));
            }
            if value_is_array {
                return Err(StylexError::shorthand_fallback());
            }
            match expansion {
                Expansion::Banned => banned(&key, options).map(Expanded::Many),
                Expansion::Alias(target) => Ok(Expanded::One((Cow::Borrowed(target), value))),
                Expansion::None => unreachable!("checked above"),
            }
        }
        StyleResolution::ApplicationOrder => {
            if key == "all" {
                if value_is_array {
                    return Err(StylexError::shorthand_fallback());
                }
                return banned(&key, options).map(Expanded::Many);
            }
            let Some(table) = application_order_expansion(&key) else {
                return Ok(Expanded::One((key, value)));
            };
            if value_is_array {
                return Err(StylexError::shorthand_fallback());
            }
            let mut pairs = Vec::with_capacity(table.len());
            pairs.push((Cow::Borrowed(table[0]), value));
            pairs.extend(table[1..].iter().map(|k| (Cow::Borrowed(*k), None)));
            Ok(Expanded::Many(pairs))
        }
        StyleResolution::LegacyExpandShorthands => {
            if !legacy_has_expansion(&key) {
                return Ok(Expanded::One((key, value)));
            }
            if value_is_array {
                return Err(StylexError::shorthand_fallback());
            }
            match legacy_expansion(&key, value) {
                Ok(pairs) => Ok(Expanded::Many(pairs)),
                Err(error) => match options.property_validation_mode {
                    PropertyValidationMode::Throw => Err(error),
                    // Warn logs upstream, then drops just like silent.
                    PropertyValidationMode::Silent | PropertyValidationMode::Warn => {
                        Ok(Expanded::Many(Vec::new()))
                    }
                },
            }
        }
    }
}

fn banned(key: &str, options: &ResolvedOptions) -> Result<Vec<ExpandedPair<'static>>, StylexError> {
    match options.property_validation_mode {
        PropertyValidationMode::Throw => Err(StylexError::banned_shorthand(key)
            .expect("banned keys are in the banned-shorthand table")),
        PropertyValidationMode::Silent | PropertyValidationMode::Warn => Ok(Vec::new()),
    }
}

enum Expansion {
    None,
    Banned,
    Alias(&'static str),
}

pub(super) fn is_unexpanded_property(key: &str, resolution: StyleResolution) -> bool {
    match resolution {
        StyleResolution::PropertySpecificity => {
            matches!(property_specificity_expansion(key), Expansion::None)
        }
        StyleResolution::ApplicationOrder => {
            key != "all" && application_order_expansion(key).is_none()
        }
        StyleResolution::LegacyExpandShorthands => !legacy_has_expansion(key),
    }
}

fn property_specificity_expansion(key: &str) -> Expansion {
    if crate::errors::banned_shorthand_message(key).is_some() {
        return Expansion::Banned;
    }
    alias_target(key).map_or(Expansion::None, Expansion::Alias)
}

// parity: property-specificity.js `aliases` (the non-throwing rewrites).
fn alias_target(key: &str) -> Option<&'static str> {
    Some(match key {
        "blockSize" => "height",
        "inlineSize" => "width",
        "minBlockSize" => "minHeight",
        "minInlineSize" => "minWidth",
        "maxBlockSize" => "maxHeight",
        "maxInlineSize" => "maxWidth",
        "borderHorizontalWidth" => "borderInlineWidth",
        "borderHorizontalStyle" => "borderInlineStyle",
        "borderHorizontalColor" => "borderInlineColor",
        "borderVerticalWidth" => "borderBlockWidth",
        "borderVerticalStyle" => "borderBlockStyle",
        "borderVerticalColor" => "borderBlockColor",
        "borderBlockStartColor" => "borderTopColor",
        "borderBlockEndColor" => "borderBottomColor",
        "borderBlockStartStyle" => "borderTopStyle",
        "borderBlockEndStyle" => "borderBottomStyle",
        "borderBlockStartWidth" => "borderTopWidth",
        "borderBlockEndWidth" => "borderBottomWidth",
        "borderStartColor" => "borderInlineStartColor",
        "borderEndColor" => "borderInlineEndColor",
        "borderStartStyle" => "borderInlineStartStyle",
        "borderEndStyle" => "borderInlineEndStyle",
        "borderStartWidth" => "borderInlineStartWidth",
        "borderEndWidth" => "borderInlineEndWidth",
        "borderTopStartRadius" => "borderStartStartRadius",
        "borderTopEndRadius" => "borderStartEndRadius",
        "borderBottomStartRadius" => "borderEndStartRadius",
        "borderBottomEndRadius" => "borderEndEndRadius",
        "containIntrinsicBlockSize" => "containIntrinsicHeight",
        "containIntrinsicInlineSize" => "containIntrinsicWidth",
        "marginBlockStart" => "marginTop",
        "marginBlockEnd" => "marginBottom",
        "marginStart" => "marginInlineStart",
        "marginEnd" => "marginInlineEnd",
        "marginHorizontal" => "marginInline",
        "marginVertical" => "marginBlock",
        "overflowBlock" => "overflowY",
        "overflowInline" => "overflowX",
        "paddingBlockStart" => "paddingTop",
        "paddingBlockEnd" => "paddingBottom",
        "paddingStart" => "paddingInlineStart",
        "paddingEnd" => "paddingInlineEnd",
        "paddingHorizontal" => "paddingInline",
        "paddingVertical" => "paddingBlock",
        "scrollMarginBlockStart" => "scrollMarginTop",
        "scrollMarginBlockEnd" => "scrollMarginBottom",
        "insetBlockStart" => "top",
        "insetBlockEnd" => "bottom",
        "start" => "insetInlineStart",
        "end" => "insetInlineEnd",
        _ => return None,
    })
}

/// application-order.js: element 0 is the property that keeps the value, the
/// rest are the longhands it nulls out. `all` is the only banned key here.
fn application_order_expansion(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "animation" => &[
            "animation",
            "animationComposition",
            "animationName",
            "animationDuration",
            "animationTimingFunction",
            "animationDelay",
            "animationIterationCount",
            "animationDirection",
            "animationFillMode",
            "animationPlayState",
            "animationRange",
            "animationRangeEnd",
            "animationRangeStart",
            "animationTimeline",
        ],
        "background" => &[
            "background",
            "backgroundAttachment",
            "backgroundClip",
            "backgroundColor",
            "backgroundImage",
            "backgroundOrigin",
            "backgroundPosition",
            "backgroundPositionX",
            "backgroundPositionY",
            "backgroundRepeat",
            "backgroundSize",
        ],
        "border" => &[
            "border",
            "borderWidth",
            "borderInlineWidth",
            "borderInlineStartWidth",
            "borderLeftWidth",
            "borderInlineEndWidth",
            "borderRightWidth",
            "borderBlockWidth",
            "borderTopWidth",
            "borderBottomWidth",
            "borderStyle",
            "borderInlineStyle",
            "borderInlineStartStyle",
            "borderLeftStyle",
            "borderInlineEndStyle",
            "borderRightStyle",
            "borderBlockStyle",
            "borderTopStyle",
            "borderBottomStyle",
            "borderColor",
            "borderInlineColor",
            "borderInlineStartColor",
            "borderLeftColor",
            "borderInlineEndColor",
            "borderRightColor",
            "borderBlockColor",
            "borderTopColor",
            "borderBottomColor",
        ],
        "borderBlock" => &[
            "borderBlock",
            "borderBlockWidth",
            "borderTopWidth",
            "borderBottomWidth",
            "borderBlockStyle",
            "borderTopStyle",
            "borderBottomStyle",
            "borderBlockColor",
            "borderTopColor",
            "borderBottomColor",
        ],
        "borderInline" => &[
            "borderInline",
            "borderInlineWidth",
            "borderInlineStartWidth",
            "borderLeftWidth",
            "borderInlineEndWidth",
            "borderRightWidth",
            "borderInlineStyle",
            "borderInlineStartStyle",
            "borderLeftStyle",
            "borderInlineEndStyle",
            "borderRightStyle",
            "borderInlineColor",
            "borderInlineStartColor",
            "borderLeftColor",
            "borderInlineEndColor",
            "borderRightColor",
        ],
        "margin" => &[
            "margin",
            "marginInline",
            "marginInlineStart",
            "marginLeft",
            "marginInlineEnd",
            "marginRight",
            "marginBlock",
            "marginTop",
            "marginBottom",
        ],
        "padding" => &[
            "padding",
            "paddingInline",
            "paddingStart",
            "paddingLeft",
            "paddingEnd",
            "paddingRight",
            "paddingBlock",
            "paddingTop",
            "paddingBottom",
        ],
        "font" => &[
            "font",
            "fontFamily",
            "fontSize",
            "fontStretch",
            "fontStyle",
            "fontVariant",
            "fontVariantAlternates",
            "fontVariantCaps",
            "fontVariantEastAsian",
            "fontVariantEmoji",
            "fontVariantLigatures",
            "fontVariantNumeric",
            "fontVariantPosition",
            "fontWeight",
            "lineHeight",
        ],
        "grid" => &[
            "grid",
            "gridTemplate",
            "gridTemplateAreas",
            "gridTemplateColumns",
            "gridTemplateRows",
            "gridAutoRows",
            "gridAutoColumns",
            "gridAutoFlow",
        ],
        "gridTemplate" => &[
            "gridTemplate",
            "gridTemplateAreas",
            "gridTemplateColumns",
            "gridTemplateRows",
        ],
        "gridArea" => &[
            "gridArea",
            "gridRow",
            "gridRowStart",
            "gridRowEnd",
            "gridColumn",
            "gridColumnStart",
            "gridColumnEnd",
        ],
        "inset" => &[
            "inset",
            "insetInline",
            "insetInlineStart",
            "insetInlineEnd",
            "left",
            "right",
            "insetBlock",
            "top",
            "bottom",
        ],
        "scrollMargin" => &[
            "scrollMargin",
            "scrollMarginBlock",
            "scrollMarginTop",
            "scrollMarginBottom",
            "scrollMarginInline",
            "scrollMarginInlineStart",
            "scrollMarginInlineEnd",
            "scrollMarginLeft",
            "scrollMarginRight",
        ],
        "scrollPadding" => &[
            "scrollPadding",
            "scrollPaddingBlock",
            "scrollPaddingTop",
            "scrollPaddingBottom",
            "scrollPaddingInline",
            "scrollPaddingInlineStart",
            "scrollPaddingInlineEnd",
            "scrollPaddingLeft",
            "scrollPaddingRight",
        ],
        "animationRange" => &["animationRange", "animationRangeEnd", "animationRangeStart"],
        "scrollTimeline" => &["scrollTimeline", "scrollTimelineName", "scrollTimelineAxis"],
        "backgroundPosition" => &[
            "backgroundPosition",
            "backgroundPositionX",
            "backgroundPositionY",
        ],
        "borderColor" => &[
            "borderColor",
            "borderInlineColor",
            "borderInlineStartColor",
            "borderLeftColor",
            "borderInlineEndColor",
            "borderRightColor",
            "borderBlockColor",
            "borderTopColor",
            "borderBottomColor",
        ],
        "borderStyle" => &[
            "borderStyle",
            "borderInlineStyle",
            "borderInlineStartStyle",
            "borderLeftStyle",
            "borderInlineEndStyle",
            "borderRightStyle",
            "borderBlockStyle",
            "borderTopStyle",
            "borderBottomStyle",
        ],
        "borderWidth" => &[
            "borderWidth",
            "borderInlineWidth",
            "borderInlineStartWidth",
            "borderLeftWidth",
            "borderInlineEndWidth",
            "borderRightWidth",
            "borderBlockWidth",
            "borderTopWidth",
            "borderBottomWidth",
        ],
        "borderBlockStart" => &[
            "borderTop",
            "borderTopWidth",
            "borderTopStyle",
            "borderTopColor",
        ],
        "borderTop" => &[
            "borderTop",
            "borderTopWidth",
            "borderTopStyle",
            "borderTopColor",
        ],
        "borderBlockEnd" => &[
            "borderBottom",
            "borderBottomWidth",
            "borderBottomStyle",
            "borderBottomColor",
        ],
        "borderBottom" => &[
            "borderBottom",
            "borderBottomWidth",
            "borderBottomStyle",
            "borderBottomColor",
        ],
        "borderInlineColor" => &[
            "borderInlineColor",
            "borderInlineStartColor",
            "borderLeftColor",
            "borderInlineEndColor",
            "borderRightColor",
        ],
        "borderInlineStyle" => &[
            "borderInlineStyle",
            "borderInlineStartStyle",
            "borderLeftStyle",
            "borderInlineEndStyle",
            "borderRightStyle",
        ],
        "borderInlineWidth" => &[
            "borderInlineWidth",
            "borderInlineStartWidth",
            "borderLeftWidth",
            "borderInlineEndWidth",
            "borderRightWidth",
        ],
        "borderInlineStart" => &[
            "borderInlineStart",
            "borderInlineStartWidth",
            "borderLeftWidth",
            "borderRightWidth",
            "borderInlineStartStyle",
            "borderLeftStyle",
            "borderRightStyle",
            "borderInlineStartColor",
            "borderLeftColor",
            "borderRightColor",
        ],
        "borderLeft" => &[
            "borderLeft",
            "borderLeftWidth",
            "borderInlineStartWidth",
            "borderInlineEndWidth",
            "borderLeftStyle",
            "borderInlineStartStyle",
            "borderInlineEndStyle",
            "borderLeftColor",
            "borderInlineStartColor",
            "borderInlineEndColor",
        ],
        "borderInlineEnd" => &[
            "borderInlineEnd",
            "borderInlineEndWidth",
            "borderLeftWidth",
            "borderRightWidth",
            "borderInlineEndStyle",
            "borderLeftStyle",
            "borderRightStyle",
            "borderInlineEndColor",
            "borderLeftColor",
            "borderRightColor",
        ],
        "borderRight" => &[
            "borderRight",
            "borderRightWidth",
            "borderInlineStartWidth",
            "borderInlineEndWidth",
            "borderRightStyle",
            "borderInlineStartStyle",
            "borderInlineEndStyle",
            "borderRightColor",
            "borderInlineStartColor",
            "borderInlineEndColor",
        ],
        "borderImage" => &[
            "borderImage",
            "borderImageOutset",
            "borderImageRepeat",
            "borderImageSlice",
            "borderImageSource",
            "borderImageWidth",
        ],
        "borderRadius" => &[
            "borderRadius",
            "borderStartStartRadius",
            "borderStartEndRadius",
            "borderEndStartRadius",
            "borderEndEndRadius",
            "borderTopLeftRadius",
            "borderTopRightRadius",
            "borderBottomLeftRadius",
            "borderBottomRightRadius",
        ],
        "cornerShape" => &[
            "cornerShape",
            "cornerStartStartShape",
            "cornerStartEndShape",
            "cornerEndStartShape",
            "cornerEndEndShape",
            "cornerTopLeftShape",
            "cornerTopRightShape",
            "cornerBottomLeftShape",
            "cornerBottomRightShape",
        ],
        "outline" => &[
            "outline",
            "outlineColor",
            "outlineOffset",
            "outlineStyle",
            "outlineWidth",
        ],
        "gridGap" => &["gap", "rowGap", "columnGap"],
        "gap" => &["gap", "rowGap", "columnGap"],
        "placeContent" => &["placeContent", "alignContent", "justifyContent"],
        "placeItems" => &["placeItems", "alignItems", "justifyItems"],
        "placeSelf" => &["placeSelf", "alignSelf", "justifySelf"],
        "marginBlock" => &["marginBlock", "marginTop", "marginBottom"],
        "marginInline" => &[
            "marginInline",
            "marginInlineStart",
            "marginLeft",
            "marginInlineEnd",
            "marginRight",
        ],
        "paddingBlock" => &["paddingBlock", "paddingTop", "paddingBottom"],
        "paddingInline" => &[
            "paddingInline",
            "paddingStart",
            "paddingLeft",
            "paddingEnd",
            "paddingRight",
        ],
        "columns" => &["columns", "columnCount", "columnWidth"],
        "columnRule" => &[
            "columnRule",
            "columnRuleColor",
            "columnRuleStyle",
            "columnRuleWidth",
        ],
        "containIntrinsicSize" => &[
            "containIntrinsicSize",
            "containIntrinsicWidth",
            "containIntrinsicHeight",
        ],
        "container" => &["container", "containerName", "containerType"],
        "flex" => &["flex", "flexGrow", "flexShrink", "flexBasis"],
        "flexFlow" => &["flexFlow", "flexDirection", "flexWrap"],
        "fontVariant" => &[
            "fontVariant",
            "fontVariantAlternates",
            "fontVariantCaps",
            "fontVariantEastAsian",
            "fontVariantEmoji",
            "fontVariantLigatures",
            "fontVariantNumeric",
            "fontVariantPosition",
        ],
        "gridRow" => &["gridRow", "gridRowStart", "gridRowEnd"],
        "gridColumn" => &["gridColumn", "gridColumnStart", "gridColumnEnd"],
        "listStyle" => &[
            "listStyle",
            "listStyleImage",
            "listStylePosition",
            "listStyleType",
        ],
        "mask" => &[
            "mask",
            "maskClip",
            "maskComposite",
            "maskImage",
            "maskMode",
            "maskOrigin",
            "maskPosition",
            "maskRepeat",
            "maskSize",
        ],
        "maskBorder" => &[
            "maskBorder",
            "maskBorderMode",
            "maskBorderOutset",
            "maskBorderRepeat",
            "maskBorderSlice",
            "maskBorderSource",
            "maskBorderWidth",
        ],
        "offset" => &[
            "offset",
            "offsetAnchor",
            "offsetDistance",
            "offsetPath",
            "offsetPosition",
            "offsetRotate",
        ],
        "overflow" => &["overflow", "overflowX", "overflowY"],
        "insetBlock" => &["insetBlock", "top", "bottom"],
        "insetInline" => &[
            "insetInline",
            "insetInlineStart",
            "insetInlineEnd",
            "left",
            "right",
        ],
        "scrollMarginBlock" => &["scrollMarginBlock", "scrollMarginTop", "scrollMarginBottom"],
        "scrollMarginInline" => &[
            "scrollMarginInline",
            "scrollMarginInlineStart",
            "scrollMarginInlineEnd",
            "scrollMarginLeft",
            "scrollMarginRight",
        ],
        "scrollPaddingBlock" => &[
            "scrollPaddingBlock",
            "scrollPaddingTop",
            "scrollPaddingBottom",
        ],
        "scrollPaddingInline" => &[
            "scrollPaddingInline",
            "scrollPaddingInlineStart",
            "scrollPaddingInlineEnd",
            "scrollPaddingLeft",
            "scrollPaddingRight",
        ],
        "scrollSnapType" => &["scrollSnapType", "scrollSnapTypeX", "scrollSnapTypeY"],
        "textDecoration" => &[
            "textDecoration",
            "textDecorationColor",
            "textDecorationLine",
            "textDecorationStyle",
            "textDecorationThickness",
        ],
        "textEmphasis" => &["textEmphasis", "textEmphasisColor", "textEmphasisStyle"],
        "transition" => &[
            "transition",
            "transitionBehavior",
            "transitionDelay",
            "transitionDuration",
            "transitionProperty",
            "transitionTimingFunction",
        ],
        "borderBlockColor" => &["borderBlockColor", "borderTopColor", "borderBottomColor"],
        "borderBlockWidth" => &["borderBlockWidth", "borderTopWidth", "borderBottomWidth"],
        "borderBlockStartColor" => &["borderTopColor"],
        "borderBlockStartStyle" => &["borderTopStyle"],
        "borderBlockStartWidth" => &["borderTopWidth"],
        "borderBlockEndColor" => &["borderBottomColor"],
        "borderBlockEndStyle" => &["borderBottomStyle"],
        "borderBlockEndWidth" => &["borderBottomWidth"],
        "borderInlineStartColor" => &[
            "borderInlineStartColor",
            "borderLeftColor",
            "borderRightColor",
        ],
        "borderInlineStartStyle" => &[
            "borderInlineStartStyle",
            "borderLeftStyle",
            "borderRightStyle",
        ],
        "borderInlineStartWidth" => &[
            "borderInlineStartWidth",
            "borderLeftWidth",
            "borderRightWidth",
        ],
        "borderInlineEndColor" => &[
            "borderInlineEndColor",
            "borderLeftColor",
            "borderRightColor",
        ],
        "borderInlineEndStyle" => &[
            "borderInlineEndStyle",
            "borderLeftStyle",
            "borderRightStyle",
        ],
        "borderInlineEndWidth" => &[
            "borderInlineEndWidth",
            "borderLeftWidth",
            "borderRightWidth",
        ],
        "borderStartEndRadius" => &[
            "borderStartEndRadius",
            "borderTopLeftRadius",
            "borderTopRightRadius",
        ],
        "borderStartStartRadius" => &[
            "borderStartStartRadius",
            "borderTopLeftRadius",
            "borderTopRightRadius",
        ],
        "borderEndEndRadius" => &[
            "borderEndEndRadius",
            "borderBottomLeftRadius",
            "borderBottomRightRadius",
        ],
        "borderEndStartRadius" => &[
            "borderEndStartRadius",
            "borderBottomLeftRadius",
            "borderBottomRightRadius",
        ],
        "cornerStartStartShape" => &[
            "cornerStartStartShape",
            "cornerTopLeftShape",
            "cornerTopRightShape",
        ],
        "cornerStartEndShape" => &[
            "cornerStartEndShape",
            "cornerTopLeftShape",
            "cornerTopRightShape",
        ],
        "cornerEndStartShape" => &[
            "cornerEndStartShape",
            "cornerBottomLeftShape",
            "cornerBottomRightShape",
        ],
        "cornerEndEndShape" => &[
            "cornerEndEndShape",
            "cornerBottomLeftShape",
            "cornerBottomRightShape",
        ],
        "gridRowGap" => &["rowGap"],
        "gridColumnGap" => &["columnGap"],
        "blockSize" => &["height"],
        "inlineSize" => &["width"],
        "maxBlockSize" => &["maxHeight"],
        "maxInlineSize" => &["maxWidth"],
        "minBlockSize" => &["minHeight"],
        "minInlineSize" => &["minWidth"],
        "marginBlockStart" => &["marginTop"],
        "marginBlockEnd" => &["marginBottom"],
        "marginInlineStart" => &["marginInlineStart", "marginLeft", "marginRight"],
        "marginInlineEnd" => &["marginInlineEnd", "marginLeft", "marginRight"],
        "paddingBlockStart" => &["paddingTop"],
        "paddingBlockEnd" => &["paddingBottom"],
        "paddingInlineStart" => &["paddingInlineStart", "paddingLeft", "paddingRight"],
        "paddingInlineEnd" => &["paddingInlineEnd", "paddingLeft", "paddingRight"],
        "containIntrinsicBlockSize" => &["containIntrinsicHeight"],
        "containIntrinsicInlineSize" => &["containIntrinsicWidth"],
        "overflowBlock" => &["overflowY"],
        "overflowInline" => &["overflowX"],
        "insetBlockStart" => &["top"],
        "insetBlockEnd" => &["bottom"],
        "insetInlineStart" => &["insetInlineStart", "left", "right"],
        "insetInlineEnd" => &["insetInlineEnd", "left", "right"],
        "scrollMarginBlockStart" => &["scrollMarginTop"],
        "scrollMarginBlockEnd" => &["scrollMarginBottom"],
        "scrollMarginInlineStart" => &[
            "scrollMarginInlineStart",
            "scrollMarginLeft",
            "scrollMarginRight",
        ],
        "scrollMarginInlineEnd" => &[
            "scrollMarginInlineEnd",
            "scrollMarginLeft",
            "scrollMarginRight",
        ],
        "scrollPaddingInlineStart" => &[
            "scrollPaddingInlineStart",
            "scrollPaddingLeft",
            "scrollPaddingRight",
        ],
        "scrollPaddingInlineEnd" => &[
            "scrollPaddingInlineEnd",
            "scrollPaddingLeft",
            "scrollPaddingRight",
        ],
        "borderLeftColor" => &[
            "borderLeftColor",
            "borderInlineStartColor",
            "borderInlineEndColor",
        ],
        "borderLeftStyle" => &[
            "borderLeftStyle",
            "borderInlineStartStyle",
            "borderInlineEndStyle",
        ],
        "borderLeftWidth" => &[
            "borderLeftWidth",
            "borderInlineStartWidth",
            "borderInlineEndWidth",
        ],
        "borderRightColor" => &[
            "borderRightColor",
            "borderInlineStartColor",
            "borderInlineEndColor",
        ],
        "borderRightStyle" => &[
            "borderRightStyle",
            "borderInlineStartStyle",
            "borderInlineEndStyle",
        ],
        "borderRightWidth" => &[
            "borderRightWidth",
            "borderInlineStartWidth",
            "borderInlineEndWidth",
        ],
        "borderTopLeftRadius" => &[
            "borderTopLeftRadius",
            "borderStartStartRadius",
            "borderStartEndRadius",
        ],
        "borderTopRightRadius" => &[
            "borderTopRightRadius",
            "borderStartStartRadius",
            "borderStartEndRadius",
        ],
        "borderBottomLeftRadius" => &[
            "borderBottomLeftRadius",
            "borderEndStartRadius",
            "borderEndEndRadius",
        ],
        "borderBottomRightRadius" => &[
            "borderBottomRightRadius",
            "borderEndStartRadius",
            "borderEndEndRadius",
        ],
        "cornerTopLeftShape" => &[
            "cornerTopLeftShape",
            "cornerStartStartShape",
            "cornerStartEndShape",
        ],
        "cornerTopRightShape" => &[
            "cornerTopRightShape",
            "cornerStartStartShape",
            "cornerStartEndShape",
        ],
        "cornerBottomLeftShape" => &[
            "cornerBottomLeftShape",
            "cornerEndStartShape",
            "cornerEndEndShape",
        ],
        "cornerBottomRightShape" => &[
            "cornerBottomRightShape",
            "cornerEndStartShape",
            "cornerEndEndShape",
        ],
        "marginLeft" => &["marginLeft", "marginInlineStart", "marginInlineEnd"],
        "marginRight" => &["marginRight", "marginInlineStart", "marginInlineEnd"],
        "paddingLeft" => &["paddingLeft", "paddingInlineStart", "paddingInlineEnd"],
        "paddingRight" => &["paddingRight", "paddingInlineStart", "paddingInlineEnd"],
        "left" => &["left", "insetInlineStart", "insetInlineEnd"],
        "right" => &["right", "insetInlineStart", "insetInlineEnd"],
        "scrollMarginLeft" => &[
            "scrollMarginLeft",
            "scrollMarginInlineStart",
            "scrollMarginInlineEnd",
        ],
        "scrollMarginRight" => &[
            "scrollMarginRight",
            "scrollMarginInlineStart",
            "scrollMarginInlineEnd",
        ],
        "scrollPaddingLeft" => &[
            "scrollPaddingLeft",
            "scrollPaddingInlineStart",
            "scrollPaddingInlineEnd",
        ],
        "scrollPaddingRight" => &[
            "scrollPaddingRight",
            "scrollPaddingInlineStart",
            "scrollPaddingInlineEnd",
        ],
        "borderHorizontal" => &[
            "borderInline",
            "borderInlineWidth",
            "borderInlineStartWidth",
            "borderLeftWidth",
            "borderInlineEndWidth",
            "borderRightWidth",
            "borderInlineStyle",
            "borderInlineStartStyle",
            "borderLeftStyle",
            "borderInlineEndStyle",
            "borderRightStyle",
            "borderInlineColor",
            "borderInlineStartColor",
            "borderLeftColor",
            "borderInlineEndColor",
            "borderRightColor",
        ],
        "borderVertical" => &[
            "borderBlock",
            "borderBlockWidth",
            "borderTopWidth",
            "borderBottomWidth",
            "borderBlockStyle",
            "borderTopStyle",
            "borderBottomStyle",
            "borderBlockColor",
            "borderTopColor",
            "borderBottomColor",
        ],
        "borderEnd" => &[
            "borderInlineEnd",
            "borderInlineEndWidth",
            "borderLeftWidth",
            "borderRightWidth",
            "borderInlineEndStyle",
            "borderLeftStyle",
            "borderRightStyle",
            "borderInlineEndColor",
            "borderLeftColor",
            "borderRightColor",
        ],
        "borderStart" => &[
            "borderInlineStart",
            "borderInlineStartWidth",
            "borderLeftWidth",
            "borderRightWidth",
            "borderInlineStartStyle",
            "borderLeftStyle",
            "borderRightStyle",
            "borderInlineStartColor",
            "borderLeftColor",
            "borderRightColor",
        ],
        "borderHorizontalWidth" => &[
            "borderInlineWidth",
            "borderInlineStartWidth",
            "borderLeftWidth",
            "borderInlineEndWidth",
            "borderRightWidth",
        ],
        "borderHorizontalStyle" => &[
            "borderInlineStyle",
            "borderInlineStartStyle",
            "borderLeftStyle",
            "borderInlineEndStyle",
            "borderRightStyle",
        ],
        "borderHorizontalColor" => &[
            "borderInlineColor",
            "borderInlineStartColor",
            "borderLeftColor",
            "borderInlineEndColor",
            "borderRightColor",
        ],
        "borderVerticalWidth" => &["borderBlockWidth", "borderTopWidth", "borderBottomWidth"],
        "borderVerticalStyle" => &["borderBlockStyle", "borderTopStyle", "borderBottomStyle"],
        "borderVerticalColor" => &["borderBlockColor", "borderTopColor", "borderBottomColor"],
        "borderStartColor" => &[
            "borderInlineStartColor",
            "borderLeftColor",
            "borderRightColor",
        ],
        "borderEndColor" => &[
            "borderInlineEndColor",
            "borderLeftColor",
            "borderRightColor",
        ],
        "borderStartStyle" => &[
            "borderInlineStartStyle",
            "borderLeftStyle",
            "borderRightStyle",
        ],
        "borderEndStyle" => &[
            "borderInlineEndStyle",
            "borderLeftStyle",
            "borderRightStyle",
        ],
        "borderStartWidth" => &[
            "borderInlineStartWidth",
            "borderLeftWidth",
            "borderRightWidth",
        ],
        "borderEndWidth" => &[
            "borderInlineEndWidth",
            "borderLeftWidth",
            "borderRightWidth",
        ],
        "borderTopStartRadius" => &["borderStartStartRadius"],
        "borderTopEndRadius" => &["borderStartEndRadius"],
        "borderBottomStartRadius" => &["borderEndStartRadius"],
        "borderBottomEndRadius" => &["borderEndEndRadius"],
        "marginStart" => &["marginInlineStart", "marginLeft", "marginRight"],
        "marginEnd" => &["marginInlineEnd", "marginLeft", "marginRight"],
        "marginHorizontal" => &[
            "marginInline",
            "marginInlineStart",
            "marginLeft",
            "marginInlineEnd",
            "marginRight",
        ],
        "marginVertical" => &["marginBlock", "marginTop", "marginBottom"],
        "paddingStart" => &["paddingInlineStart", "paddingLeft", "paddingRight"],
        "paddingEnd" => &["paddingInlineEnd", "paddingLeft", "paddingRight"],
        "paddingHorizontal" => &[
            "paddingInline",
            "paddingStart",
            "paddingLeft",
            "paddingEnd",
            "paddingRight",
        ],
        "paddingVertical" => &["paddingBlock", "paddingTop", "paddingBottom"],
        "start" => &["insetInlineStart", "left", "right"],
        "end" => &["insetInlineEnd", "left", "right"],
        "borderBlockStyle" => &["borderBlockStyle", "borderTopStyle", "borderBottomStyle"],
        _ => return None,
    })
}

fn legacy_has_expansion(key: &str) -> bool {
    matches!(
        key,
        "border"
            | "borderColor"
            | "borderStyle"
            | "borderWidth"
            | "borderRadius"
            | "borderHorizontal"
            | "borderVertical"
            | "borderHorizontalColor"
            | "borderHorizontalStyle"
            | "borderHorizontalWidth"
            | "borderVerticalColor"
            | "borderVerticalStyle"
            | "borderVerticalWidth"
            | "containIntrinsicSize"
            | "inset"
            | "insetInline"
            | "insetBlock"
            | "start"
            | "end"
            | "left"
            | "right"
            | "gap"
            | "listStyle"
            | "margin"
            | "marginHorizontal"
            | "marginStart"
            | "marginEnd"
            | "marginLeft"
            | "marginRight"
            | "marginVertical"
            | "overflow"
            | "padding"
            | "paddingHorizontal"
            | "paddingStart"
            | "paddingEnd"
            | "paddingLeft"
            | "paddingRight"
            | "paddingVertical"
            | "insetBlockStart"
            | "insetBlockEnd"
            | "insetInlineStart"
            | "insetInlineEnd"
            | "blockSize"
            | "inlineSize"
            | "minBlockSize"
            | "minInlineSize"
            | "maxBlockSize"
            | "maxInlineSize"
            | "borderStart"
            | "borderEnd"
            | "borderBlockWidth"
            | "borderBlockStyle"
            | "borderBlockColor"
            | "borderBlockStartWidth"
            | "borderBlockStartStyle"
            | "borderBlockStartColor"
            | "borderBlockEndWidth"
            | "borderBlockEndStyle"
            | "borderBlockEndColor"
            | "borderInlineWidth"
            | "borderInlineStyle"
            | "borderInlineColor"
            | "borderTopStartRadius"
            | "borderTopEndRadius"
            | "borderBottomStartRadius"
            | "borderBottomEndRadius"
            | "gridGap"
            | "gridRowGap"
            | "gridColumnGap"
            | "marginBlock"
            | "marginBlockStart"
            | "marginBlockEnd"
            | "marginInline"
            | "overflowBlock"
            | "overflowInline"
            | "paddingBlock"
            | "paddingBlockStart"
            | "paddingBlockEnd"
            | "paddingInline"
            | "scrollMarginBlockStart"
            | "scrollMarginBlockEnd"
            | "float"
            | "clear"
    )
}

fn one<'a>(key: &'static str, value: Option<StyleScalar<'a>>) -> Vec<ExpandedPair<'a>> {
    vec![pair(key, value)]
}

fn pair<'a>(key: impl Into<Cow<'a, str>>, value: Option<StyleScalar<'a>>) -> ExpandedPair<'a> {
    (key.into(), value)
}

/// JS array destructuring: a missing index reads as `undefined`, which the
/// `= t` / `= r` defaults then replace.
fn at<'a>(parts: &[Option<StyleScalar<'a>>], index: usize) -> Option<StyleScalar<'a>> {
    parts
        .get(index)
        .cloned()
        .unwrap_or(Some(StyleScalar::Undefined))
}

fn or<'a>(
    value: Option<StyleScalar<'a>>,
    fallback: &Option<StyleScalar<'a>>,
) -> Option<StyleScalar<'a>> {
    if value == Some(StyleScalar::Undefined) {
        fallback.clone()
    } else {
        value
    }
}

/// `[t, r = t, b = t, l = r]` over the split parts.
fn box_sides<'a>(parts: &[Option<StyleScalar<'a>>]) -> [Option<StyleScalar<'a>>; 4] {
    let t = at(parts, 0);
    let r = or(at(parts, 1), &t);
    let b = or(at(parts, 2), &t);
    let l = or(at(parts, 3), &r);
    [t, r, b, l]
}

/// `[a, b = a]` over the split parts.
fn two_sides<'a>(parts: &[Option<StyleScalar<'a>>]) -> [Option<StyleScalar<'a>>; 2] {
    let a = at(parts, 0);
    let b = or(at(parts, 1), &a);
    [a, b]
}

fn quad<'a>(keys: [&'static str; 4], parts: &[Option<StyleScalar<'a>>]) -> Vec<ExpandedPair<'a>> {
    let [t, r, b, l] = box_sides(parts);
    vec![
        pair(keys[0], t),
        pair(keys[1], r),
        pair(keys[2], b),
        pair(keys[3], l),
    ]
}

fn duo<'a>(keys: [&'static str; 2], parts: &[Option<StyleScalar<'a>>]) -> Vec<ExpandedPair<'a>> {
    let [a, b] = two_sides(parts);
    vec![pair(keys[0], a), pair(keys[1], b)]
}

fn same<'a>(keys: [&'static str; 2], value: Option<StyleScalar<'a>>) -> Vec<ExpandedPair<'a>> {
    vec![pair(keys[0], value.clone()), pair(keys[1], value)]
}

/// `key: v` plus the two physical longhands it must null out.
fn punched<'a>(
    key: &'static str,
    value: Option<StyleScalar<'a>>,
    nulls: [&'static str; 2],
) -> Vec<ExpandedPair<'a>> {
    vec![pair(key, value), pair(nulls[0], None), pair(nulls[1], None)]
}

// parity: legacy-expand-shorthands.js `shorthands` merged under `aliases`.
fn legacy_expansion<'a>(
    key: &Cow<'a, str>,
    value: Option<StyleScalar<'a>>,
) -> Result<Vec<ExpandedPair<'a>>, StylexError> {
    let parts = || split_value(value.as_ref());
    Ok(match key.as_ref() {
        "border" => vec![
            pair("borderTop", value.clone()),
            pair("borderInlineEnd", value.clone()),
            pair("borderBottom", value.clone()),
            pair("borderInlineStart", value.clone()),
        ],
        "borderColor" => quad(
            [
                "borderTopColor",
                "borderInlineEndColor",
                "borderBottomColor",
                "borderInlineStartColor",
            ],
            &parts(),
        ),
        "borderStyle" => quad(
            [
                "borderTopStyle",
                "borderInlineEndStyle",
                "borderBottomStyle",
                "borderInlineStartStyle",
            ],
            &parts(),
        ),
        "borderWidth" => quad(
            [
                "borderTopWidth",
                "borderInlineEndWidth",
                "borderBottomWidth",
                "borderInlineStartWidth",
            ],
            &parts(),
        ),
        "borderRadius" => quad(
            [
                "borderStartStartRadius",
                "borderStartEndRadius",
                "borderEndEndRadius",
                "borderEndStartRadius",
            ],
            &parts(),
        ),
        "borderHorizontal" => same(["borderInlineStart", "borderInlineEnd"], value),
        "borderVertical" => same(["borderTop", "borderBottom"], value),
        "borderHorizontalColor" | "borderInlineColor" => {
            same(["borderInlineStartColor", "borderInlineEndColor"], value)
        }
        "borderHorizontalStyle" | "borderInlineStyle" => {
            same(["borderInlineStartStyle", "borderInlineEndStyle"], value)
        }
        "borderHorizontalWidth" | "borderInlineWidth" => {
            same(["borderInlineStartWidth", "borderInlineEndWidth"], value)
        }
        "borderVerticalColor" | "borderBlockColor" => {
            same(["borderTopColor", "borderBottomColor"], value)
        }
        "borderVerticalStyle" | "borderBlockStyle" => {
            same(["borderTopStyle", "borderBottomStyle"], value)
        }
        "borderVerticalWidth" | "borderBlockWidth" => {
            same(["borderTopWidth", "borderBottomWidth"], value)
        }
        "containIntrinsicSize" => contain_intrinsic_size(&parts()),
        "inset" => quad(
            ["top", "insetInlineEnd", "bottom", "insetInlineStart"],
            &parts(),
        ),
        "insetInline" => {
            let [s, e] = two_sides(&parts());
            let mut out = legacy_start(s);
            out.extend(legacy_end(e));
            out
        }
        "insetBlock" => duo(["top", "bottom"], &parts()),
        "start" | "insetInlineStart" => legacy_start(value.clone()),
        "end" | "insetInlineEnd" => legacy_end(value.clone()),
        "left" => punched("left", value, ["insetInlineStart", "insetInlineEnd"]),
        "right" => punched("right", value, ["insetInlineStart", "insetInlineEnd"]),
        "gap" | "gridGap" => duo(["rowGap", "columnGap"], &parts()),
        "listStyle" => list_style(value)?,
        "margin" => quad(
            [
                "marginTop",
                "marginInlineEnd",
                "marginBottom",
                "marginInlineStart",
            ],
            &parts(),
        ),
        "marginHorizontal" | "marginInline" => {
            let [s, e] = two_sides(&parts());
            let mut out = punched_owned("marginInlineStart", s, ["marginLeft", "marginRight"]);
            out.extend(punched_owned(
                "marginInlineEnd",
                e,
                ["marginLeft", "marginRight"],
            ));
            out
        }
        "marginStart" => punched("marginInlineStart", value, ["marginLeft", "marginRight"]),
        "marginEnd" => punched("marginInlineEnd", value, ["marginLeft", "marginRight"]),
        "marginLeft" => punched(
            "marginLeft",
            value,
            ["marginInlineStart", "marginInlineEnd"],
        ),
        "marginRight" => punched(
            "marginRight",
            value,
            ["marginInlineStart", "marginInlineEnd"],
        ),
        "marginVertical" | "marginBlock" => duo(["marginTop", "marginBottom"], &parts()),
        "overflow" => duo(["overflowX", "overflowY"], &parts()),
        "padding" => quad(
            [
                "paddingTop",
                "paddingInlineEnd",
                "paddingBottom",
                "paddingInlineStart",
            ],
            &parts(),
        ),
        "paddingHorizontal" | "paddingInline" => {
            let [s, e] = two_sides(&parts());
            let mut out = punched_owned("paddingInlineStart", s, ["paddingLeft", "paddingRight"]);
            out.extend(punched_owned(
                "paddingInlineEnd",
                e,
                ["paddingLeft", "paddingRight"],
            ));
            out
        }
        "paddingStart" => punched("paddingInlineStart", value, ["paddingLeft", "paddingRight"]),
        "paddingEnd" => punched("paddingInlineEnd", value, ["paddingLeft", "paddingRight"]),
        "paddingLeft" => punched(
            "paddingLeft",
            value,
            ["paddingInlineStart", "paddingInlineEnd"],
        ),
        "paddingRight" => punched(
            "paddingRight",
            value,
            ["paddingInlineStart", "paddingInlineEnd"],
        ),
        "paddingVertical" | "paddingBlock" => duo(["paddingTop", "paddingBottom"], &parts()),
        "insetBlockStart" | "marginBlockStart" | "paddingBlockStart" => {
            one(block_start_target(key), value)
        }
        "insetBlockEnd" | "marginBlockEnd" | "paddingBlockEnd" => one(block_end_target(key), value),
        "blockSize" => one("height", value),
        "inlineSize" => one("width", value),
        "minBlockSize" => one("minHeight", value),
        "minInlineSize" => one("minWidth", value),
        "maxBlockSize" => one("maxHeight", value),
        "maxInlineSize" => one("maxWidth", value),
        "borderStart" => one("borderInlineStart", value),
        "borderEnd" => one("borderInlineEnd", value),
        "borderBlockStartWidth" => one("borderTopWidth", value),
        "borderBlockStartStyle" => one("borderTopStyle", value),
        "borderBlockStartColor" => one("borderTopColor", value),
        "borderBlockEndWidth" => one("borderBottomWidth", value),
        "borderBlockEndStyle" => one("borderBottomStyle", value),
        "borderBlockEndColor" => one("borderBottomColor", value),
        "borderTopStartRadius" => one("borderStartStartRadius", value),
        "borderTopEndRadius" => one("borderStartEndRadius", value),
        "borderBottomStartRadius" => one("borderEndStartRadius", value),
        "borderBottomEndRadius" => one("borderEndEndRadius", value),
        "gridRowGap" => one("rowGap", value),
        "gridColumnGap" => one("columnGap", value),
        "overflowBlock" => one("overflowY", value),
        "overflowInline" => one("overflowX", value),
        "scrollMarginBlockStart" => one("scrollMarginTop", value),
        "scrollMarginBlockEnd" => one("scrollMarginBottom", value),
        "float" | "clear" => vec![pair(key.clone(), logical_float_value(value.as_ref()))],
        _ => unreachable!("legacy_has_expansion gates this match"),
    })
}

fn block_start_target(key: &str) -> &'static str {
    match key {
        "insetBlockStart" => "top",
        "marginBlockStart" => "marginTop",
        _ => "paddingTop",
    }
}

fn block_end_target(key: &str) -> &'static str {
    match key {
        "insetBlockEnd" => "bottom",
        "marginBlockEnd" => "marginBottom",
        _ => "paddingBottom",
    }
}

fn punched_owned<'a>(
    key: &'static str,
    value: Option<StyleScalar<'a>>,
    nulls: [&'static str; 2],
) -> Vec<ExpandedPair<'a>> {
    vec![pair(key, value), pair(nulls[0], None), pair(nulls[1], None)]
}

fn legacy_start(value: Option<StyleScalar>) -> Vec<ExpandedPair> {
    punched_owned("insetInlineStart", value, ["left", "right"])
}

fn legacy_end(value: Option<StyleScalar>) -> Vec<ExpandedPair> {
    punched_owned("insetInlineEnd", value, ["left", "right"])
}

fn logical_float_value<'a>(value: Option<&StyleScalar<'a>>) -> Option<StyleScalar<'a>> {
    match value {
        Some(StyleScalar::Str(s)) if s == "inline-start" || s == "start" => {
            Some(StyleScalar::Str(Cow::Borrowed(LOGICAL_START_VAR)))
        }
        Some(StyleScalar::Str(s)) if s == "inline-end" || s == "end" => {
            Some(StyleScalar::Str(Cow::Borrowed(LOGICAL_END_VAR)))
        }
        other => other.cloned(),
    }
}

// parity: legacy-expand-shorthands.js containIntrinsicSize — an `auto` part
// absorbs the part after it.
fn contain_intrinsic_size<'a>(parts: &[Option<StyleScalar<'a>>]) -> Vec<ExpandedPair<'a>> {
    let mut folded: Vec<Option<StyleScalar>> = Vec::with_capacity(parts.len());
    for part in parts {
        let absorbs = matches!(folded.last(), Some(Some(StyleScalar::Str(s))) if s == "auto")
            && !matches!(part, None | Some(StyleScalar::Undefined));
        if absorbs {
            let joined = format!("auto {}", scalar_text(part.as_ref()));
            folded.pop();
            folded.push(Some(StyleScalar::Str(Cow::Owned(joined))));
        } else {
            folded.push(part.clone());
        }
    }
    let [w, h] = two_sides(&folded);
    vec![
        pair("containIntrinsicWidth", w),
        pair("containIntrinsicHeight", h),
    ]
}

/// `${part}` template coercion.
fn scalar_text(value: Option<&StyleScalar<'_>>) -> String {
    match value {
        Some(StyleScalar::Str(s)) => s.to_string(),
        Some(StyleScalar::Num(n)) => crate::jsrt::js_number_to_string(*n),
        Some(StyleScalar::Undefined) => "undefined".to_string(),
        None => "null".to_string(),
    }
}

const LIST_STYLE_GLOBALS: [&str; 4] = ["inherit", "initial", "revert", "unset"];

// parity: legacy-expand-shorthands.js listStyle — the one throwing expansion.
fn list_style<'a>(value: Option<StyleScalar<'a>>) -> Result<Vec<ExpandedPair<'a>>, StylexError> {
    let nulls = || {
        vec![
            pair("listStyleType", None),
            pair("listStylePosition", None),
            pair("listStyleImage", None),
        ]
    };
    let Some(value) = value else {
        return Ok(nulls());
    };
    let parts = split_value(Some(&value));
    let strings: Vec<Option<&str>> = parts
        .iter()
        .map(|p| match p {
            Some(StyleScalar::Str(s)) => Some(s.as_ref()),
            _ => None,
        })
        .collect();
    if strings.len() == 1
        && let Some(only) = strings[0]
        && LIST_STYLE_GLOBALS.contains(&only)
    {
        return Ok(vec![
            pair(
                "listStyleType",
                Some(StyleScalar::Str(Cow::Owned(only.to_string()))),
            ),
            pair(
                "listStylePosition",
                Some(StyleScalar::Str(Cow::Owned(only.to_string()))),
            ),
            pair(
                "listStyleImage",
                Some(StyleScalar::Str(Cow::Owned(only.to_string()))),
            ),
        ]);
    }

    let mut ty: Option<&str> = None;
    let mut position: Option<&str> = None;
    let mut image: Option<&str> = None;
    let mut remaining: Vec<&str> = Vec::new();
    for part in strings.iter().flatten() {
        if LIST_STYLE_GLOBALS.contains(part) || part.contains("var(--") {
            return Err(list_style_error(&value, true));
        }
        if *part == "inside" || *part == "outside" {
            if position.is_some() {
                return Err(list_style_error(&value, false));
            }
            position = Some(part);
        } else if *part != "none" && is_list_style_type(part) {
            if ty.is_some() {
                return Err(list_style_error(&value, false));
            }
            ty = Some(part);
        } else {
            remaining.push(part);
        }
    }
    for part in remaining {
        if part == "none" && ty.is_none() {
            ty = Some(part);
        } else {
            if image.is_some() {
                return Err(list_style_error(&value, false));
            }
            image = Some(part);
        }
    }
    let cell = |v: Option<&str>| v.map(|s| StyleScalar::Str(Cow::Owned(s.to_string())));
    Ok(vec![
        pair("listStyleType", cell(ty)),
        pair("listStylePosition", cell(position)),
        pair("listStyleImage", cell(image)),
    ])
}

/// `/^([a-z-]+|".*?"|'.*?')$/` — `.` never matches a JS line terminator.
fn is_list_style_type(part: &str) -> bool {
    if !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') {
        return true;
    }
    let mut chars = part.chars();
    let (Some(first), Some(last)) = (chars.next(), chars.next_back()) else {
        return false;
    };
    (first == '"' || first == '\'')
        && last == first
        && !chars.any(|c| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
}

// parity: the two message spellings differ only by an extra pair of quotes.
fn list_style_error(value: &StyleScalar, doubled_quotes: bool) -> StylexError {
    let json = match value {
        StyleScalar::Str(s) => serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\"")),
        StyleScalar::Num(n) => crate::jsrt::js_number_to_string(*n),
        StyleScalar::Undefined => "undefined".to_string(),
    };
    StylexError::invalid_list_style(&if doubled_quotes {
        format!("\"{json}\"")
    } else {
        json
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ErrorCode;

    fn s(v: &str) -> StyleScalar<'_> {
        StyleScalar::Str(Cow::Owned(v.to_string()))
    }

    fn expand(key: &str, value: &str, options: &ResolvedOptions) -> Vec<(String, String)> {
        flat_map_expanded_shorthands(Cow::Borrowed(key), Some(s(value)), false, options)
            .unwrap()
            .into_iter()
            .map(|(k, v)| {
                (
                    k.into_owned(),
                    match v {
                        Some(StyleScalar::Str(text)) => text.into_owned(),
                        Some(other) => format!("{other:?}"),
                        None => "null".to_string(),
                    },
                )
            })
            .collect()
    }

    fn legacy() -> ResolvedOptions {
        ResolvedOptions {
            style_resolution: StyleResolution::LegacyExpandShorthands,
            ..ResolvedOptions::default()
        }
    }

    fn application_order() -> ResolvedOptions {
        ResolvedOptions {
            style_resolution: StyleResolution::ApplicationOrder,
            ..ResolvedOptions::default()
        }
    }

    #[test]
    fn aliases_rewrite_and_pass_value_through() {
        let opts = ResolvedOptions::default();
        assert_eq!(
            expand("marginStart", "1px", &opts),
            vec![("marginInlineStart".to_string(), "1px".to_string())]
        );
        assert_eq!(
            expand("insetBlockStart", "1px", &opts),
            vec![("top".to_string(), "1px".to_string())]
        );
        assert_eq!(
            expand("color", "red", &opts),
            vec![("color".to_string(), "red".to_string())]
        );
        assert_eq!(
            expand("var(--myVar)", "1px", &opts),
            vec![("--myVar".to_string(), "1px".to_string())]
        );
        assert_eq!(
            expand("var(--abc", "1px", &opts),
            vec![("var(--abc".to_string(), "1px".to_string())]
        );
    }

    #[test]
    fn banned_shorthands_drop_silently_and_throw_in_throw_mode() {
        let silent = ResolvedOptions::default();
        assert!(expand("border", "1px", &silent).is_empty());
        let throw = ResolvedOptions {
            property_validation_mode: PropertyValidationMode::Throw,
            ..ResolvedOptions::default()
        };
        let err = flat_map_expanded_shorthands(
            Cow::Borrowed("borderStart"),
            Some(s("1px")),
            false,
            &throw,
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::BannedShorthand);
        assert_eq!(
            err.message,
            "borderInlineStart is not supported. Use borderInlineStartWidth, borderInlineStartStyle and borderInlineStartColor instead."
        );
    }

    #[test]
    fn warn_mode_drops_exactly_like_silent() {
        let warn = ResolvedOptions {
            property_validation_mode: PropertyValidationMode::Warn,
            ..ResolvedOptions::default()
        };
        let silent = ResolvedOptions::default();
        for key in ["border", "borderStart", "all", "listStyle", "animation"] {
            assert_eq!(
                flat_map_expanded_shorthands(Cow::Borrowed(key), Some(s("1px")), false, &warn)
                    .unwrap(),
                flat_map_expanded_shorthands(Cow::Borrowed(key), Some(s("1px")), false, &silent)
                    .unwrap(),
                "key: {key}"
            );
        }
        // The array-value throw is orthogonal to the mode.
        assert_eq!(
            flat_map_expanded_shorthands(Cow::Borrowed("marginStart"), Some(s("1px")), true, &warn)
                .unwrap_err()
                .code,
            ErrorCode::ShorthandFallback
        );
    }

    #[test]
    fn shorthand_with_array_value_throws_regardless_of_mode() {
        let opts = ResolvedOptions::default();
        let err =
            flat_map_expanded_shorthands(Cow::Borrowed("marginStart"), Some(s("1px")), true, &opts)
                .unwrap_err();
        assert_eq!(err.code, ErrorCode::ShorthandFallback);
        assert_eq!(
            flat_map_expanded_shorthands(Cow::Borrowed("color"), Some(s("red")), true, &opts)
                .unwrap(),
            Expanded::One((Cow::Borrowed("color"), Some(s("red"))))
        );
    }

    #[test]
    fn application_order_keeps_the_shorthand_and_punches_the_longhands() {
        let opts = application_order();
        assert_eq!(
            expand("gridGap", "1px", &opts),
            vec![
                ("gap".to_string(), "1px".to_string()),
                ("rowGap".to_string(), "null".to_string()),
                ("columnGap".to_string(), "null".to_string()),
            ]
        );
        // Only `all` stays banned; every property-specificity ban passes through.
        assert_eq!(
            expand("border", "1px", &opts),
            expand("border", "1px", &opts)
        );
        assert_eq!(expand("border", "1px", &opts)[0].0, "border");
        assert!(expand("all", "initial", &opts).is_empty());
        let throw = ResolvedOptions {
            property_validation_mode: PropertyValidationMode::Throw,
            ..application_order()
        };
        assert_eq!(
            flat_map_expanded_shorthands(Cow::Borrowed("all"), Some(s("initial")), false, &throw)
                .unwrap_err()
                .code,
            ErrorCode::BannedShorthand
        );
        assert_eq!(
            flat_map_expanded_shorthands(
                Cow::Borrowed("animation"),
                Some(s("a 1s")),
                false,
                &throw
            )
            .unwrap()
            .len(),
            14
        );
    }

    #[test]
    fn legacy_splits_values_and_punches_nulls() {
        let opts = legacy();
        assert_eq!(
            expand("margin", "1px 2px 3px 4px", &opts),
            vec![
                ("marginTop".to_string(), "1px".to_string()),
                ("marginInlineEnd".to_string(), "2px".to_string()),
                ("marginBottom".to_string(), "3px".to_string()),
                ("marginInlineStart".to_string(), "4px".to_string()),
            ]
        );
        assert_eq!(
            expand("start", "1px", &opts),
            vec![
                ("insetInlineStart".to_string(), "1px".to_string()),
                ("left".to_string(), "null".to_string()),
                ("right".to_string(), "null".to_string()),
            ]
        );
        // insetInline is start ++ end, so left/right appear twice before dedupe.
        assert_eq!(expand("insetInline", "1px", &opts).len(), 6);
        // The `/` is a div node, so a two-radius value fills all four corners.
        assert_eq!(
            expand("borderRadius", "10px / 20px", &opts),
            vec![
                ("borderStartStartRadius".to_string(), "10px".to_string()),
                ("borderStartEndRadius".to_string(), "20px".to_string()),
                ("borderEndEndRadius".to_string(), "10px".to_string()),
                ("borderEndStartRadius".to_string(), "20px".to_string()),
            ]
        );
        assert_eq!(
            expand("float", "start", &opts),
            vec![("float".to_string(), LOGICAL_START_VAR.to_string())]
        );
        assert_eq!(
            expand("clear", "left", &opts),
            vec![("clear".to_string(), "left".to_string())]
        );
        // No banned shorthands at all under legacy, even in throw mode.
        let throw = ResolvedOptions {
            property_validation_mode: PropertyValidationMode::Throw,
            ..legacy()
        };
        assert_eq!(
            expand("all", "initial", &throw),
            vec![("all".to_string(), "initial".to_string())]
        );
        assert_eq!(expand("borderTop", "1px", &throw)[0].0, "borderTop");
    }

    #[test]
    fn legacy_empty_value_yields_undefined_longhands() {
        let opts = legacy();
        let pairs =
            flat_map_expanded_shorthands(Cow::Borrowed("margin"), Some(s("   ")), false, &opts)
                .unwrap();
        assert_eq!(pairs.len(), 4);
        assert!(
            pairs
                .iter()
                .all(|(_, v)| *v == Some(StyleScalar::Undefined))
        );
        let nulled =
            flat_map_expanded_shorthands(Cow::Borrowed("margin"), None, false, &opts).unwrap();
        assert!(nulled.iter().all(|(_, v)| v.is_none()));
    }

    #[test]
    fn contain_intrinsic_size_folds_auto() {
        let opts = legacy();
        assert_eq!(
            expand("containIntrinsicSize", "auto 10px auto 20px", &opts),
            vec![
                ("containIntrinsicWidth".to_string(), "auto 10px".to_string()),
                (
                    "containIntrinsicHeight".to_string(),
                    "auto 20px".to_string()
                ),
            ]
        );
        assert_eq!(
            expand("containIntrinsicSize", "10px auto 20px", &opts),
            vec![
                ("containIntrinsicWidth".to_string(), "10px".to_string()),
                (
                    "containIntrinsicHeight".to_string(),
                    "auto 20px".to_string()
                ),
            ]
        );
    }

    #[test]
    fn list_style_parses_and_throws() {
        let opts = legacy();
        assert_eq!(
            expand(
                "listStyle",
                "outside \"+\" linear-gradient(90deg, white 100%)",
                &opts
            ),
            vec![
                ("listStyleType".to_string(), "\"+\"".to_string()),
                ("listStylePosition".to_string(), "outside".to_string()),
                (
                    "listStyleImage".to_string(),
                    "linear-gradient(90deg,white 100%)".to_string()
                ),
            ]
        );
        assert_eq!(
            expand("listStyle", "none none", &opts),
            vec![
                ("listStyleType".to_string(), "none".to_string()),
                ("listStylePosition".to_string(), "null".to_string()),
                ("listStyleImage".to_string(), "none".to_string()),
            ]
        );
        assert_eq!(
            expand("listStyle", "inherit", &opts),
            vec![
                ("listStyleType".to_string(), "inherit".to_string()),
                ("listStylePosition".to_string(), "inherit".to_string()),
                ("listStyleImage".to_string(), "inherit".to_string()),
            ]
        );
        // Numbers are skipped by both passes, so every longhand comes back null.
        let numeric = flat_map_expanded_shorthands(
            Cow::Borrowed("listStyle"),
            Some(StyleScalar::Num(5.0)),
            false,
            &opts,
        )
        .unwrap();
        assert!(numeric.iter().all(|(_, v)| v.is_none()));
        // Silent mode drops the property whole.
        assert!(expand("listStyle", "none inherit", &opts).is_empty());
        let throw = ResolvedOptions {
            property_validation_mode: PropertyValidationMode::Throw,
            ..legacy()
        };
        let err = flat_map_expanded_shorthands(
            Cow::Borrowed("listStyle"),
            Some(s("none inherit")),
            false,
            &throw,
        )
        .unwrap_err();
        assert_eq!(
            err.message,
            "invalid \"listStyle\" value of \"\"none inherit\"\""
        );
        let err = flat_map_expanded_shorthands(
            Cow::Borrowed("listStyle"),
            Some(s("square circle")),
            false,
            &throw,
        )
        .unwrap_err();
        assert_eq!(
            err.message,
            "invalid \"listStyle\" value of \"square circle\""
        );
    }
}
