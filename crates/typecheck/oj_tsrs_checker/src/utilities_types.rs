//! Non-function declarations of `utilities.go`.

use std::sync::LazyLock;

use crate::*;

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum AssignmentKind {
    #[default]
    None,
    Definite,
    Compound,
}

// BinaryExpression | PrefixUnaryExpression | PostfixUnaryExpression | ForInOrOfStatement

/// orderedSetMapThreshold is the size at which an orderedSet materializes its dedup map. Below this,
/// `contains()` scans the values slice.
pub(crate) const ORDERED_SET_MAP_THRESHOLD: usize = 16;

pub struct CheckerOrderedSet<T: Copy + Eq + std::hash::Hash> {
    pub(crate) values_by_key: Option<FxHashSet<T>>,
    pub(crate) values: Vec<T>,
}

impl<T: Copy + Eq + std::hash::Hash> Default for CheckerOrderedSet<T> {
    fn default() -> Self {
        CheckerOrderedSet { values_by_key: None, values: Vec::new() }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FeatureMapEntry {
    pub(crate) lib: &'static str,
    pub(crate) props: &'static [&'static str],
}

/// Go `var getFeatureMap = sync.OnceValue(...)`; call as `get_feature_map()`.
pub(crate) fn get_feature_map() -> &'static FxHashMap<&'static str, Vec<FeatureMapEntry>> {
    static MAP: LazyLock<FxHashMap<&'static str, Vec<FeatureMapEntry>>> =
        LazyLock::new(build_feature_map);
    &MAP
}

fn build_feature_map() -> FxHashMap<&'static str, Vec<FeatureMapEntry>> {
    let e = |lib: &'static str, props: &'static [&'static str]| FeatureMapEntry { lib, props };
    FxHashMap::from_iter(
        FEATURE_MAP.iter().map(|(name, entries)| {
            (*name, entries.iter().map(|(lib, props)| e(lib, props)).collect())
        }),
    )
}

type FeatureList = &'static [(&'static str, &'static [&'static str])];

static FEATURE_MAP: &[(&str, FeatureList)] = &[
    (
        "Array",
        &[
            ("es2015", &["find", "findIndex", "fill", "copyWithin", "entries", "keys", "values"]),
            ("es2016", &["includes"]),
            ("es2019", &["flat", "flatMap"]),
            ("es2022", &["at"]),
            (
                "es2023",
                &["findLastIndex", "findLast", "toReversed", "toSorted", "toSpliced", "with"],
            ),
        ],
    ),
    ("Iterator", &[("es2015", &[])]),
    ("IteratorConstructor", &[("es2026", &["concat"])]),
    ("RawJSON", &[("es2026", &[])]),
    ("JSON", &[("es2026", &["isRawJSON", "rawJSON"])]),
    ("AsyncIterator", &[("es2015", &[])]),
    (
        "ArrayBuffer",
        &[(
            "es2024",
            &[
                "maxByteLength",
                "resizable",
                "resize",
                "detached",
                "transfer",
                "transferToFixedLength",
            ],
        )],
    ),
    (
        "Atomics",
        &[
            (
                "es2017",
                &[
                    "add",
                    "and",
                    "compareExchange",
                    "exchange",
                    "isLockFree",
                    "load",
                    "or",
                    "store",
                    "sub",
                    "wait",
                    "notify",
                    "xor",
                ],
            ),
            ("es2024", &["waitAsync"]),
        ],
    ),
    (
        "SharedArrayBuffer",
        &[("es2017", &["byteLength", "slice"]), ("es2024", &["growable", "maxByteLength", "grow"])],
    ),
    ("AsyncIterable", &[("es2018", &[])]),
    ("AsyncIterableIterator", &[("es2018", &[])]),
    ("AsyncGenerator", &[("es2018", &[])]),
    ("AsyncGeneratorFunction", &[("es2018", &[])]),
    (
        "RegExp",
        &[
            ("es2015", &["flags", "sticky", "unicode"]),
            ("es2018", &["dotAll"]),
            ("es2024", &["unicodeSets"]),
        ],
    ),
    ("RegExpConstructor", &[("es2025", &["escape"])]),
    (
        "Reflect",
        &[(
            "es2015",
            &[
                "apply",
                "construct",
                "defineProperty",
                "deleteProperty",
                "get",
                "getOwnPropertyDescriptor",
                "getPrototypeOf",
                "has",
                "isExtensible",
                "ownKeys",
                "preventExtensions",
                "set",
                "setPrototypeOf",
            ],
        )],
    ),
    ("ArrayConstructor", &[("es2015", &["from", "of"]), ("es2026", &["fromAsync"])]),
    (
        "ObjectConstructor",
        &[
            ("es2015", &["assign", "getOwnPropertySymbols", "keys", "is", "setPrototypeOf"]),
            ("es2017", &["values", "entries", "getOwnPropertyDescriptors"]),
            ("es2019", &["fromEntries"]),
            ("es2022", &["hasOwn"]),
            ("es2024", &["groupBy"]),
        ],
    ),
    (
        "NumberConstructor",
        &[(
            "es2015",
            &["isFinite", "isInteger", "isNaN", "isSafeInteger", "parseFloat", "parseInt"],
        )],
    ),
    (
        "Math",
        &[
            (
                "es2015",
                &[
                    "clz32", "imul", "sign", "log10", "log2", "log1p", "expm1", "cosh", "sinh",
                    "tanh", "acosh", "asinh", "atanh", "hypot", "trunc", "fround", "cbrt",
                ],
            ),
            ("es2025", &["f16round"]),
            ("es2026", &["sumPrecise"]),
        ],
    ),
    (
        "Map",
        &[
            ("es2015", &["entries", "keys", "values"]),
            ("es2026", &["getOrInsert", "getOrInsertComputed"]),
        ],
    ),
    ("MapConstructor", &[("es2024", &["groupBy"])]),
    (
        "Set",
        &[
            ("es2015", &["entries", "keys", "values"]),
            (
                "es2025",
                &[
                    "union",
                    "intersection",
                    "difference",
                    "symmetricDifference",
                    "isSubsetOf",
                    "isSupersetOf",
                    "isDisjointFrom",
                ],
            ),
        ],
    ),
    (
        "PromiseConstructor",
        &[
            ("es2015", &["all", "race", "reject", "resolve"]),
            ("es2020", &["allSettled"]),
            ("es2021", &["any"]),
            ("es2024", &["withResolvers"]),
            ("es2025", &["try"]),
        ],
    ),
    ("Symbol", &[("es2015", &["for", "keyFor"]), ("es2019", &["description"])]),
    ("WeakMap", &[("es2015", &[]), ("es2026", &["getOrInsert", "getOrInsertComputed"])]),
    ("WeakSet", &[("es2015", &[])]),
    (
        "String",
        &[
            (
                "es2015",
                &[
                    "codePointAt",
                    "includes",
                    "endsWith",
                    "normalize",
                    "repeat",
                    "startsWith",
                    "anchor",
                    "big",
                    "blink",
                    "bold",
                    "fixed",
                    "fontcolor",
                    "fontsize",
                    "italics",
                    "link",
                    "small",
                    "strike",
                    "sub",
                    "sup",
                ],
            ),
            ("es2017", &["padStart", "padEnd"]),
            ("es2019", &["trimStart", "trimEnd", "trimLeft", "trimRight"]),
            ("es2020", &["matchAll"]),
            ("es2021", &["replaceAll"]),
            ("es2022", &["at"]),
            ("es2024", &["isWellFormed", "toWellFormed"]),
        ],
    ),
    ("StringConstructor", &[("es2015", &["fromCodePoint", "raw"])]),
    ("DateTimeFormat", &[("es2017", &["formatToParts"])]),
    ("Promise", &[("es2015", &[]), ("es2018", &["finally"])]),
    ("RegExpMatchArray", &[("es2018", &["groups"])]),
    ("RegExpExecArray", &[("es2018", &["groups"])]),
    (
        "Intl",
        &[
            ("es2018", &["PluralRules"]),
            ("es2020", &["RelativeTimeFormat", "Locale", "DisplayNames"]),
            ("es2021", &["ListFormat", "DateTimeFormat"]),
            ("es2022", &["Segmenter"]),
            ("es2025", &["DurationFormat"]),
        ],
    ),
    ("NumberFormat", &[("es2018", &["formatToParts"])]),
    (
        "SymbolConstructor",
        &[("es2020", &["matchAll"]), ("esnext", &["metadata", "dispose", "asyncDispose"])],
    ),
    (
        "DataView",
        &[
            ("es2020", &["setBigInt64", "setBigUint64", "getBigInt64", "getBigUint64"]),
            ("es2025", &["setFloat16", "getFloat16"]),
        ],
    ),
    ("BigInt", &[("es2020", &[])]),
    ("RelativeTimeFormat", &[("es2020", &["format", "formatToParts", "resolvedOptions"])]),
    (
        "Int8Array",
        &[
            ("es2022", &["at"]),
            (
                "es2023",
                &["findLastIndex", "findLast", "toReversed", "toSorted", "toSpliced", "with"],
            ),
        ],
    ),
    (
        "Uint8Array",
        &[
            ("es2022", &["at"]),
            (
                "es2023",
                &["findLastIndex", "findLast", "toReversed", "toSorted", "toSpliced", "with"],
            ),
            ("es2026", &["toBase64", "setFromBase64", "toHex", "setFromHex"]),
        ],
    ),
    (
        "Uint8ClampedArray",
        &[
            ("es2022", &["at"]),
            (
                "es2023",
                &["findLastIndex", "findLast", "toReversed", "toSorted", "toSpliced", "with"],
            ),
        ],
    ),
    (
        "Int16Array",
        &[
            ("es2022", &["at"]),
            (
                "es2023",
                &["findLastIndex", "findLast", "toReversed", "toSorted", "toSpliced", "with"],
            ),
        ],
    ),
    (
        "Uint16Array",
        &[
            ("es2022", &["at"]),
            (
                "es2023",
                &["findLastIndex", "findLast", "toReversed", "toSorted", "toSpliced", "with"],
            ),
        ],
    ),
    (
        "Int32Array",
        &[
            ("es2022", &["at"]),
            (
                "es2023",
                &["findLastIndex", "findLast", "toReversed", "toSorted", "toSpliced", "with"],
            ),
        ],
    ),
    (
        "Uint32Array",
        &[
            ("es2022", &["at"]),
            (
                "es2023",
                &["findLastIndex", "findLast", "toReversed", "toSorted", "toSpliced", "with"],
            ),
        ],
    ),
    ("Float16Array", &[("es2025", &[])]),
    (
        "Float32Array",
        &[
            ("es2022", &["at"]),
            (
                "es2023",
                &["findLastIndex", "findLast", "toReversed", "toSorted", "toSpliced", "with"],
            ),
        ],
    ),
    (
        "Float64Array",
        &[
            ("es2022", &["at"]),
            (
                "es2023",
                &["findLastIndex", "findLast", "toReversed", "toSorted", "toSpliced", "with"],
            ),
        ],
    ),
    (
        "BigInt64Array",
        &[
            ("es2020", &[]),
            ("es2022", &["at"]),
            (
                "es2023",
                &["findLastIndex", "findLast", "toReversed", "toSorted", "toSpliced", "with"],
            ),
        ],
    ),
    (
        "BigUint64Array",
        &[
            ("es2020", &[]),
            ("es2022", &["at"]),
            (
                "es2023",
                &["findLastIndex", "findLast", "toReversed", "toSorted", "toSpliced", "with"],
            ),
        ],
    ),
    ("Error", &[("es2022", &["cause"])]),
    ("ErrorConstructor", &[("es2026", &["isError"])]),
    ("Uint8ArrayConstructor", &[("es2026", &["fromBase64", "fromHex"])]),
    ("DisposableStack", &[("esnext", &[])]),
    ("AsyncDisposableStack", &[("esnext", &[])]),
    ("Date", &[("esnext", &["toTemporalInstant"])]),
];

/// `DiagnosticDetails` holds a resolved diagnostic message and its arguments, used for sharing diagnostic
/// chain computation between the checker and incremental builder.
#[derive(Clone, Debug)]
pub struct DiagnosticDetails {
    pub message: &'static Message,
    pub args: Vec<String>,
}
