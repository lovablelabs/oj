use std::sync::LazyLock;

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_core::collections::OrderedMap;
use tsrs_core::tspath;
use tsrs_core::{
    CompilerOptions, JsxEmit, ModuleDetectionKind, ModuleKind, ModuleResolutionKind, NewLineKind,
    ScriptTarget,
};

use crate::commandlineoption::CompilerOptionsValue;

fn new_map(
    entries: &[(&'static str, CompilerOptionsValue)],
) -> OrderedMap<&'static str, CompilerOptionsValue> {
    let mut m = OrderedMap::default();
    m.extend(entries.iter().map(|(k, v)| (*k, v.clone())));
    m
}

pub(crate) static LIB_MAP: LazyLock<OrderedMap<&'static str, CompilerOptionsValue>> =
    LazyLock::new(|| {
        new_map(&[
            // JavaScript only
            ("es5", CompilerOptionsValue::String("lib.es5.d.ts".to_string())),
            ("es6", CompilerOptionsValue::String("lib.es2015.d.ts".to_string())),
            ("es2015", CompilerOptionsValue::String("lib.es2015.d.ts".to_string())),
            ("es7", CompilerOptionsValue::String("lib.es2016.d.ts".to_string())),
            ("es2016", CompilerOptionsValue::String("lib.es2016.d.ts".to_string())),
            ("es2017", CompilerOptionsValue::String("lib.es2017.d.ts".to_string())),
            ("es2018", CompilerOptionsValue::String("lib.es2018.d.ts".to_string())),
            ("es2019", CompilerOptionsValue::String("lib.es2019.d.ts".to_string())),
            ("es2020", CompilerOptionsValue::String("lib.es2020.d.ts".to_string())),
            ("es2021", CompilerOptionsValue::String("lib.es2021.d.ts".to_string())),
            ("es2022", CompilerOptionsValue::String("lib.es2022.d.ts".to_string())),
            ("es2023", CompilerOptionsValue::String("lib.es2023.d.ts".to_string())),
            ("es2024", CompilerOptionsValue::String("lib.es2024.d.ts".to_string())),
            ("es2025", CompilerOptionsValue::String("lib.es2025.d.ts".to_string())),
            ("es2026", CompilerOptionsValue::String("lib.es2026.d.ts".to_string())),
            ("esnext", CompilerOptionsValue::String("lib.esnext.d.ts".to_string())),
            // Host only
            ("dom", CompilerOptionsValue::String("lib.dom.d.ts".to_string())),
            ("dom.iterable", CompilerOptionsValue::String("lib.dom.iterable.d.ts".to_string())),
            (
                "dom.asynciterable",
                CompilerOptionsValue::String("lib.dom.asynciterable.d.ts".to_string()),
            ),
            ("webworker", CompilerOptionsValue::String("lib.webworker.d.ts".to_string())),
            (
                "webworker.importscripts",
                CompilerOptionsValue::String("lib.webworker.importscripts.d.ts".to_string()),
            ),
            (
                "webworker.iterable",
                CompilerOptionsValue::String("lib.webworker.iterable.d.ts".to_string()),
            ),
            (
                "webworker.asynciterable",
                CompilerOptionsValue::String("lib.webworker.asynciterable.d.ts".to_string()),
            ),
            ("scripthost", CompilerOptionsValue::String("lib.scripthost.d.ts".to_string())),
            // ES2015 and later By-feature options
            ("es2015.core", CompilerOptionsValue::String("lib.es2015.core.d.ts".to_string())),
            (
                "es2015.collection",
                CompilerOptionsValue::String("lib.es2015.collection.d.ts".to_string()),
            ),
            (
                "es2015.generator",
                CompilerOptionsValue::String("lib.es2015.generator.d.ts".to_string()),
            ),
            (
                "es2015.iterable",
                CompilerOptionsValue::String("lib.es2015.iterable.d.ts".to_string()),
            ),
            ("es2015.promise", CompilerOptionsValue::String("lib.es2015.promise.d.ts".to_string())),
            ("es2015.proxy", CompilerOptionsValue::String("lib.es2015.proxy.d.ts".to_string())),
            ("es2015.reflect", CompilerOptionsValue::String("lib.es2015.reflect.d.ts".to_string())),
            ("es2015.symbol", CompilerOptionsValue::String("lib.es2015.symbol.d.ts".to_string())),
            (
                "es2015.symbol.wellknown",
                CompilerOptionsValue::String("lib.es2015.symbol.wellknown.d.ts".to_string()),
            ),
            (
                "es2016.array.include",
                CompilerOptionsValue::String("lib.es2016.array.include.d.ts".to_string()),
            ),
            ("es2016.intl", CompilerOptionsValue::String("lib.es2016.intl.d.ts".to_string())),
            (
                "es2017.arraybuffer",
                CompilerOptionsValue::String("lib.es2017.arraybuffer.d.ts".to_string()),
            ),
            ("es2017.date", CompilerOptionsValue::String("lib.es2017.date.d.ts".to_string())),
            ("es2017.object", CompilerOptionsValue::String("lib.es2017.object.d.ts".to_string())),
            (
                "es2017.sharedmemory",
                CompilerOptionsValue::String("lib.es2017.sharedmemory.d.ts".to_string()),
            ),
            ("es2017.string", CompilerOptionsValue::String("lib.es2017.string.d.ts".to_string())),
            ("es2017.intl", CompilerOptionsValue::String("lib.es2017.intl.d.ts".to_string())),
            (
                "es2017.typedarrays",
                CompilerOptionsValue::String("lib.es2017.typedarrays.d.ts".to_string()),
            ),
            (
                "es2018.asyncgenerator",
                CompilerOptionsValue::String("lib.es2018.asyncgenerator.d.ts".to_string()),
            ),
            (
                "es2018.asynciterable",
                CompilerOptionsValue::String("lib.es2018.asynciterable.d.ts".to_string()),
            ),
            ("es2018.intl", CompilerOptionsValue::String("lib.es2018.intl.d.ts".to_string())),
            ("es2018.promise", CompilerOptionsValue::String("lib.es2018.promise.d.ts".to_string())),
            ("es2018.regexp", CompilerOptionsValue::String("lib.es2018.regexp.d.ts".to_string())),
            ("es2019.array", CompilerOptionsValue::String("lib.es2019.array.d.ts".to_string())),
            ("es2019.object", CompilerOptionsValue::String("lib.es2019.object.d.ts".to_string())),
            ("es2019.string", CompilerOptionsValue::String("lib.es2019.string.d.ts".to_string())),
            ("es2019.symbol", CompilerOptionsValue::String("lib.es2019.symbol.d.ts".to_string())),
            ("es2019.intl", CompilerOptionsValue::String("lib.es2019.intl.d.ts".to_string())),
            ("es2020.bigint", CompilerOptionsValue::String("lib.es2020.bigint.d.ts".to_string())),
            ("es2020.date", CompilerOptionsValue::String("lib.es2020.date.d.ts".to_string())),
            ("es2020.promise", CompilerOptionsValue::String("lib.es2020.promise.d.ts".to_string())),
            (
                "es2020.sharedmemory",
                CompilerOptionsValue::String("lib.es2020.sharedmemory.d.ts".to_string()),
            ),
            ("es2020.string", CompilerOptionsValue::String("lib.es2020.string.d.ts".to_string())),
            (
                "es2020.symbol.wellknown",
                CompilerOptionsValue::String("lib.es2020.symbol.wellknown.d.ts".to_string()),
            ),
            ("es2020.intl", CompilerOptionsValue::String("lib.es2020.intl.d.ts".to_string())),
            ("es2020.number", CompilerOptionsValue::String("lib.es2020.number.d.ts".to_string())),
            ("es2021.promise", CompilerOptionsValue::String("lib.es2021.promise.d.ts".to_string())),
            ("es2021.string", CompilerOptionsValue::String("lib.es2021.string.d.ts".to_string())),
            ("es2021.weakref", CompilerOptionsValue::String("lib.es2021.weakref.d.ts".to_string())),
            ("es2021.intl", CompilerOptionsValue::String("lib.es2021.intl.d.ts".to_string())),
            ("es2022.array", CompilerOptionsValue::String("lib.es2022.array.d.ts".to_string())),
            ("es2022.error", CompilerOptionsValue::String("lib.es2022.error.d.ts".to_string())),
            ("es2022.intl", CompilerOptionsValue::String("lib.es2022.intl.d.ts".to_string())),
            ("es2022.object", CompilerOptionsValue::String("lib.es2022.object.d.ts".to_string())),
            ("es2022.string", CompilerOptionsValue::String("lib.es2022.string.d.ts".to_string())),
            ("es2022.regexp", CompilerOptionsValue::String("lib.es2022.regexp.d.ts".to_string())),
            ("es2023.array", CompilerOptionsValue::String("lib.es2023.array.d.ts".to_string())),
            (
                "es2023.collection",
                CompilerOptionsValue::String("lib.es2023.collection.d.ts".to_string()),
            ),
            ("es2023.intl", CompilerOptionsValue::String("lib.es2023.intl.d.ts".to_string())),
            (
                "es2024.arraybuffer",
                CompilerOptionsValue::String("lib.es2024.arraybuffer.d.ts".to_string()),
            ),
            (
                "es2024.collection",
                CompilerOptionsValue::String("lib.es2024.collection.d.ts".to_string()),
            ),
            ("es2024.object", CompilerOptionsValue::String("lib.es2024.object.d.ts".to_string())),
            ("es2024.promise", CompilerOptionsValue::String("lib.es2024.promise.d.ts".to_string())),
            ("es2024.regexp", CompilerOptionsValue::String("lib.es2024.regexp.d.ts".to_string())),
            (
                "es2024.sharedmemory",
                CompilerOptionsValue::String("lib.es2024.sharedmemory.d.ts".to_string()),
            ),
            ("es2024.string", CompilerOptionsValue::String("lib.es2024.string.d.ts".to_string())),
            (
                "es2025.collection",
                CompilerOptionsValue::String("lib.es2025.collection.d.ts".to_string()),
            ),
            ("es2025.float16", CompilerOptionsValue::String("lib.es2025.float16.d.ts".to_string())),
            ("es2025.intl", CompilerOptionsValue::String("lib.es2025.intl.d.ts".to_string())),
            (
                "es2025.iterator",
                CompilerOptionsValue::String("lib.es2025.iterator.d.ts".to_string()),
            ),
            ("es2025.promise", CompilerOptionsValue::String("lib.es2025.promise.d.ts".to_string())),
            ("es2025.regexp", CompilerOptionsValue::String("lib.es2025.regexp.d.ts".to_string())),
            ("es2026.array", CompilerOptionsValue::String("lib.es2026.array.d.ts".to_string())),
            (
                "es2026.collection",
                CompilerOptionsValue::String("lib.es2026.collection.d.ts".to_string()),
            ),
            ("es2026.error", CompilerOptionsValue::String("lib.es2026.error.d.ts".to_string())),
            (
                "es2026.iterator",
                CompilerOptionsValue::String("lib.es2026.iterator.d.ts".to_string()),
            ),
            ("es2026.json", CompilerOptionsValue::String("lib.es2026.json.d.ts".to_string())),
            ("es2026.math", CompilerOptionsValue::String("lib.es2026.math.d.ts".to_string())),
            (
                "es2026.typedarrays",
                CompilerOptionsValue::String("lib.es2026.typedarrays.d.ts".to_string()),
            ),
            // Fallback for backward compatibility
            (
                "esnext.asynciterable",
                CompilerOptionsValue::String("lib.es2018.asynciterable.d.ts".to_string()),
            ),
            ("esnext.symbol", CompilerOptionsValue::String("lib.es2019.symbol.d.ts".to_string())),
            ("esnext.bigint", CompilerOptionsValue::String("lib.es2020.bigint.d.ts".to_string())),
            ("esnext.weakref", CompilerOptionsValue::String("lib.es2021.weakref.d.ts".to_string())),
            ("esnext.object", CompilerOptionsValue::String("lib.es2024.object.d.ts".to_string())),
            ("esnext.regexp", CompilerOptionsValue::String("lib.es2024.regexp.d.ts".to_string())),
            ("esnext.string", CompilerOptionsValue::String("lib.es2024.string.d.ts".to_string())),
            ("esnext.float16", CompilerOptionsValue::String("lib.es2025.float16.d.ts".to_string())),
            ("esnext.array", CompilerOptionsValue::String("lib.es2026.array.d.ts".to_string())),
            (
                "esnext.collection",
                CompilerOptionsValue::String("lib.es2026.collection.d.ts".to_string()),
            ),
            ("esnext.error", CompilerOptionsValue::String("lib.es2026.error.d.ts".to_string())),
            (
                "esnext.typedarrays",
                CompilerOptionsValue::String("lib.es2026.typedarrays.d.ts".to_string()),
            ),
            (
                "esnext.iterator",
                CompilerOptionsValue::String("lib.esnext.iterator.d.ts".to_string()),
            ),
            ("esnext.promise", CompilerOptionsValue::String("lib.esnext.promise.d.ts".to_string())),
            // ESNext By-feature options
            ("esnext.date", CompilerOptionsValue::String("lib.esnext.date.d.ts".to_string())),
            (
                "esnext.decorators",
                CompilerOptionsValue::String("lib.esnext.decorators.d.ts".to_string()),
            ),
            (
                "esnext.disposable",
                CompilerOptionsValue::String("lib.esnext.disposable.d.ts".to_string()),
            ),
            ("esnext.intl", CompilerOptionsValue::String("lib.esnext.intl.d.ts".to_string())),
            (
                "esnext.modulesource",
                CompilerOptionsValue::String("lib.esnext.modulesource.d.ts".to_string()),
            ),
            (
                "esnext.sharedmemory",
                CompilerOptionsValue::String("lib.esnext.sharedmemory.d.ts".to_string()),
            ),
            (
                "esnext.temporal",
                CompilerOptionsValue::String("lib.esnext.temporal.d.ts".to_string()),
            ),
            // Decorators
            ("decorators", CompilerOptionsValue::String("lib.decorators.d.ts".to_string())),
            (
                "decorators.legacy",
                CompilerOptionsValue::String("lib.decorators.legacy.d.ts".to_string()),
            ),
        ])
    });

pub static LIBS: LazyLock<Vec<&'static str>> = LazyLock::new(|| LIB_MAP.keys().copied().collect());
static LIB_FILES_SET: LazyLock<FxHashSet<String>> =
    LazyLock::new(|| LIB_MAP.values().map(|s| s.as_str().unwrap().to_string()).collect());

pub fn get_lib_file_name(lib_name: &str) -> Option<String> {
    // checks if the libName is a valid lib name or file name and converts the lib name to the filename if needed
    let lib_name = tspath::to_file_name_lower_case(lib_name);
    if LIB_FILES_SET.contains(lib_name.as_str()) {
        return Some(lib_name);
    }
    let lib = LIB_MAP.get(lib_name.as_str())?;
    Some(lib.as_str().unwrap().to_string())
}

pub(crate) static MODULE_RESOLUTION_OPTION_MAP: LazyLock<
    OrderedMap<&'static str, CompilerOptionsValue>,
> = LazyLock::new(|| {
    new_map(&[
        ("node16", CompilerOptionsValue::ModuleResolutionKind(ModuleResolutionKind::NODE16)),
        ("nodenext", CompilerOptionsValue::ModuleResolutionKind(ModuleResolutionKind::NODE_NEXT)),
        ("bundler", CompilerOptionsValue::ModuleResolutionKind(ModuleResolutionKind::BUNDLER)),
        ("classic", CompilerOptionsValue::ModuleResolutionKind(ModuleResolutionKind::CLASSIC)),
        ("node", CompilerOptionsValue::ModuleResolutionKind(ModuleResolutionKind::NODE10)),
        ("node10", CompilerOptionsValue::ModuleResolutionKind(ModuleResolutionKind::NODE10)),
    ])
});

pub(crate) static TARGET_OPTION_MAP: LazyLock<OrderedMap<&'static str, CompilerOptionsValue>> =
    LazyLock::new(|| {
        new_map(&[
            ("es5", CompilerOptionsValue::ScriptTarget(ScriptTarget::ES5)),
            ("es6", CompilerOptionsValue::ScriptTarget(ScriptTarget::ES2015)),
            ("es2015", CompilerOptionsValue::ScriptTarget(ScriptTarget::ES2015)),
            ("es2016", CompilerOptionsValue::ScriptTarget(ScriptTarget::ES2016)),
            ("es2017", CompilerOptionsValue::ScriptTarget(ScriptTarget::ES2017)),
            ("es2018", CompilerOptionsValue::ScriptTarget(ScriptTarget::ES2018)),
            ("es2019", CompilerOptionsValue::ScriptTarget(ScriptTarget::ES2019)),
            ("es2020", CompilerOptionsValue::ScriptTarget(ScriptTarget::ES2020)),
            ("es2021", CompilerOptionsValue::ScriptTarget(ScriptTarget::ES2021)),
            ("es2022", CompilerOptionsValue::ScriptTarget(ScriptTarget::ES2022)),
            ("es2023", CompilerOptionsValue::ScriptTarget(ScriptTarget::ES2023)),
            ("es2024", CompilerOptionsValue::ScriptTarget(ScriptTarget::ES2024)),
            ("es2025", CompilerOptionsValue::ScriptTarget(ScriptTarget::ES2025)),
            ("es2026", CompilerOptionsValue::ScriptTarget(ScriptTarget::ES2026)),
            ("esnext", CompilerOptionsValue::ScriptTarget(ScriptTarget::ES_NEXT)),
        ])
    });

pub(crate) static MODULE_OPTION_MAP: LazyLock<OrderedMap<&'static str, CompilerOptionsValue>> =
    LazyLock::new(|| {
        new_map(&[
            ("commonjs", CompilerOptionsValue::ModuleKind(ModuleKind::COMMON_JS)),
            ("amd", CompilerOptionsValue::ModuleKind(ModuleKind::AMD)),
            ("system", CompilerOptionsValue::ModuleKind(ModuleKind::SYSTEM)),
            ("umd", CompilerOptionsValue::ModuleKind(ModuleKind::UMD)),
            ("es6", CompilerOptionsValue::ModuleKind(ModuleKind::ES2015)),
            ("es2015", CompilerOptionsValue::ModuleKind(ModuleKind::ES2015)),
            ("es2020", CompilerOptionsValue::ModuleKind(ModuleKind::ES2020)),
            ("es2022", CompilerOptionsValue::ModuleKind(ModuleKind::ES2022)),
            ("esnext", CompilerOptionsValue::ModuleKind(ModuleKind::ES_NEXT)),
            ("node16", CompilerOptionsValue::ModuleKind(ModuleKind::NODE16)),
            ("node18", CompilerOptionsValue::ModuleKind(ModuleKind::NODE18)),
            ("node20", CompilerOptionsValue::ModuleKind(ModuleKind::NODE20)),
            ("nodenext", CompilerOptionsValue::ModuleKind(ModuleKind::NODE_NEXT)),
            ("preserve", CompilerOptionsValue::ModuleKind(ModuleKind::PRESERVE)),
        ])
    });

pub(crate) static MODULE_DETECTION_OPTION_MAP: LazyLock<
    OrderedMap<&'static str, CompilerOptionsValue>,
> = LazyLock::new(|| {
    new_map(&[
        ("auto", CompilerOptionsValue::ModuleDetectionKind(ModuleDetectionKind::Auto)),
        ("legacy", CompilerOptionsValue::ModuleDetectionKind(ModuleDetectionKind::Legacy)),
        ("force", CompilerOptionsValue::ModuleDetectionKind(ModuleDetectionKind::Force)),
    ])
});

pub(crate) static JSX_OPTION_MAP: LazyLock<OrderedMap<&'static str, CompilerOptionsValue>> =
    LazyLock::new(|| {
        new_map(&[
            ("preserve", CompilerOptionsValue::JsxEmit(JsxEmit::PRESERVE)),
            ("react-native", CompilerOptionsValue::JsxEmit(JsxEmit::REACT_NATIVE)),
            ("react-jsx", CompilerOptionsValue::JsxEmit(JsxEmit::REACT_JSX)),
            ("react-jsxdev", CompilerOptionsValue::JsxEmit(JsxEmit::REACT_JSX_DEV)),
            ("react", CompilerOptionsValue::JsxEmit(JsxEmit::REACT)),
        ])
    });

pub(crate) static NEW_LINE_OPTION_MAP: LazyLock<OrderedMap<&'static str, CompilerOptionsValue>> =
    LazyLock::new(|| {
        new_map(&[
            ("crlf", CompilerOptionsValue::NewLineKind(NewLineKind::CRLF)),
            ("lf", CompilerOptionsValue::NewLineKind(NewLineKind::LF)),
        ])
    });

static TARGET_TO_LIB_MAP: LazyLock<FxHashMap<ScriptTarget, &'static str>> = LazyLock::new(|| {
    let mut m = FxHashMap::default();
    m.insert(ScriptTarget::ES_NEXT, "lib.esnext.full.d.ts");
    m.insert(ScriptTarget::ES2026, "lib.es2026.full.d.ts");
    m.insert(ScriptTarget::ES2025, "lib.es2025.full.d.ts");
    m.insert(ScriptTarget::ES2024, "lib.es2024.full.d.ts");
    m.insert(ScriptTarget::ES2023, "lib.es2023.full.d.ts");
    m.insert(ScriptTarget::ES2022, "lib.es2022.full.d.ts");
    m.insert(ScriptTarget::ES2021, "lib.es2021.full.d.ts");
    m.insert(ScriptTarget::ES2020, "lib.es2020.full.d.ts");
    m.insert(ScriptTarget::ES2019, "lib.es2019.full.d.ts");
    m.insert(ScriptTarget::ES2018, "lib.es2018.full.d.ts");
    m.insert(ScriptTarget::ES2017, "lib.es2017.full.d.ts");
    m.insert(ScriptTarget::ES2016, "lib.es2016.full.d.ts");
    m.insert(ScriptTarget::ES2015, "lib.es6.d.ts"); // We don't use lib.es2015.full.d.ts due to breaking change.
    m
});

pub fn get_default_lib_file_name(options: &CompilerOptions) -> &'static str {
    match TARGET_TO_LIB_MAP.get(&options.get_emit_script_target()) {
        Some(name) => name,
        None => "lib.d.ts",
    }
}
