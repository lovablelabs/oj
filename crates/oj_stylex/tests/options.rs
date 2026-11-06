// Expectations re-expressed from @stylexjs/babel-plugin@0.19
// src/utils/__tests__/state-manager-test.js plus the v1 hard-error matrix.
use oj_stylex::errors::{ErrorCode, StylexError};
use oj_stylex::eval::value::{EvalValue, JsObjectMap};
use oj_stylex::options::{
    CompilerOptions, ImportSource, ModuleResolution, ModuleResolutionType, PropertyValidationMode,
    ResolvedOptions, StyleResolution, resolve_options,
};
use serde_json::{Value, json};

fn resolve(v: Value) -> Result<ResolvedOptions, StylexError> {
    CompilerOptions::from_json(&v)?.resolve()
}

fn ok(v: Value) -> ResolvedOptions {
    resolve(v).expect("options should resolve")
}

fn err_code(v: Value) -> ErrorCode {
    resolve(v).expect_err("options should hard-error").code
}

#[test]
fn defaults_match_upstream_resolution() {
    let opts = ok(json!({}));
    assert_eq!(opts.class_name_prefix, "x");
    assert!(!opts.dev);
    assert!(!opts.debug);
    assert!(!opts.test);
    assert!(!opts.enable_debug_class_names);
    assert!(!opts.enable_debug_data_prop);
    assert!(!opts.enable_dev_class_names);
    assert!(!opts.enable_font_size_px_to_rem);
    assert!(opts.enable_inlined_conditional_merge);
    assert!(opts.enable_media_query_order);
    assert!(opts.enable_minified_keys);
    assert!(!opts.enable_legacy_value_flipping);
    assert!(!opts.enable_logical_styles_polyfill);
    assert!(!opts.enable_ltr_rtl_comments);
    assert_eq!(opts.sx_prop_name.as_deref(), Some("sx"));
    assert_eq!(specifiers(&opts), vec!["@stylexjs/stylex", "stylex"]);
    assert_eq!(opts.runtime_injection, None);
    assert_eq!(opts.style_resolution, StyleResolution::PropertySpecificity);
    assert_eq!(
        opts.property_validation_mode,
        PropertyValidationMode::Silent
    );
    assert!(!opts.treeshake_compensation);
    assert_eq!(opts.unstable_module_resolution, None);

    assert_eq!(ok(Value::Null), opts);
    assert_eq!(ResolvedOptions::default(), opts);
    assert_eq!(resolve_options(&CompilerOptions::default()).unwrap(), opts);
}

type BoolChainRow = (Value, fn(&ResolvedOptions) -> bool, bool);

#[test]
fn boolean_chains_resolve_like_set_options() {
    let table: Vec<BoolChainRow> = vec![
        (json!({ "dev": true }), |o| o.dev, true),
        (json!({ "dev": false }), |o| o.dev, false),
        // debug follows dev unless given explicitly.
        (json!({ "dev": true }), |o| o.debug, true),
        (json!({ "dev": true, "debug": false }), |o| o.debug, false),
        (json!({ "debug": true }), |o| o.debug, true),
        (json!({ "debug": true }), |o| o.dev, false),
        // enableDebugDataProp follows debug.
        (json!({ "debug": true }), |o| o.enable_debug_data_prop, true),
        (
            json!({ "debug": true, "enableDebugDataProp": false }),
            |o| o.enable_debug_data_prop,
            false,
        ),
        (
            json!({ "enableDebugDataProp": true }),
            |o| o.enable_debug_data_prop,
            true,
        ),
        (
            json!({ "dev": true, "debug": false }),
            |o| o.enable_debug_data_prop,
            false,
        ),
        // enableDebugClassNames defaults false even under debug.
        (
            json!({ "debug": true }),
            |o| o.enable_debug_class_names,
            false,
        ),
        (
            json!({ "debug": true, "enableDebugClassNames": true }),
            |o| o.enable_debug_class_names,
            true,
        ),
        (
            json!({ "debug": true, "enableDebugClassNames": false }),
            |o| o.enable_debug_class_names,
            false,
        ),
        // enableDevClassNames follows dev, not debug.
        (json!({ "dev": true }), |o| o.enable_dev_class_names, true),
        (
            json!({ "debug": true }),
            |o| o.enable_dev_class_names,
            false,
        ),
        (
            json!({ "dev": true, "enableDevClassNames": false }),
            |o| o.enable_dev_class_names,
            false,
        ),
        (
            json!({ "enableDevClassNames": true }),
            |o| o.enable_dev_class_names,
            true,
        ),
        (
            json!({ "enableInlinedConditionalMerge": false }),
            |o| o.enable_inlined_conditional_merge,
            false,
        ),
        (
            json!({ "enableInlinedConditionalMerge": true }),
            |o| o.enable_inlined_conditional_merge,
            true,
        ),
        (
            json!({ "enableMinifiedKeys": false }),
            |o| o.enable_minified_keys,
            false,
        ),
        (
            json!({ "enableMediaQueryOrder": false }),
            |o| o.enable_media_query_order,
            false,
        ),
        (json!({ "test": true }), |o| o.test, true),
        (json!({ "test": false }), |o| o.test, false),
        (
            json!({ "treeshakeCompensation": true }),
            |o| o.treeshake_compensation,
            true,
        ),
        // Explicit null reads as absent (upstream `??`).
        (json!({ "debug": null, "dev": true }), |o| o.debug, true),
    ];
    for (input, extract, expected) in table {
        let text = input.to_string();
        assert_eq!(extract(&ok(input)), expected, "input: {text}");
    }
}

#[test]
fn class_name_prefix_and_sx_prop_name() {
    assert_eq!(
        ok(json!({ "classNamePrefix": "prefix" })).class_name_prefix,
        "prefix"
    );
    // Upstream accepts an empty prefix without complaint.
    assert_eq!(ok(json!({ "classNamePrefix": "" })).class_name_prefix, "");
    assert_eq!(
        ok(json!({ "sxPropName": "css" })).sx_prop_name.as_deref(),
        Some("css")
    );
    assert_eq!(ok(json!({ "sxPropName": false })).sx_prop_name, None);
}

fn specifiers(opts: &ResolvedOptions) -> Vec<&str> {
    opts.import_sources
        .iter()
        .map(ImportSource::from_specifier)
        .collect()
}

#[test]
fn import_sources_prepend_builtin_defaults() {
    assert_eq!(
        specifiers(&ok(json!({ "importSources": [] }))),
        vec!["@stylexjs/stylex", "stylex"]
    );
    assert_eq!(
        specifiers(&ok(json!({ "importSources": ["built-with-stylex"] }))),
        vec!["@stylexjs/stylex", "stylex", "built-with-stylex"]
    );
}

#[test]
fn import_sources_accept_the_from_as_object_form() {
    let opts = ok(json!({
        "importSources": ["plain-lib", { "from": "react-strict-dom", "as": "css", "extra": 1 }]
    }));
    // z.object strips keys outside the shape; the projection is `from` only.
    assert_eq!(
        opts.import_sources[3],
        ImportSource::Aliased {
            from: "react-strict-dom".to_string(),
            as_name: "css".to_string(),
        }
    );
    assert_eq!(
        specifiers(&opts),
        vec![
            "@stylexjs/stylex",
            "stylex",
            "plain-lib",
            "react-strict-dom"
        ]
    );
    // The first configured entry is always index 2 (the sx-synthesis fallback).
    assert_eq!(opts.import_source_at(2), Some("plain-lib"));
    assert!(opts.is_import_source("react-strict-dom"));
    assert_eq!(opts.import_as("react-strict-dom"), Some("css"));
    // importAs skips string entries, and unknown sources have no alias.
    assert_eq!(opts.import_as("plain-lib"), None);
    assert_eq!(opts.import_as("@stylexjs/stylex"), None);

    // An object entry beats a string entry for the same `from`, either order,
    // and the first object entry wins over a later duplicate.
    for order in [
        json!([{ "from": "my-lib", "as": "a" }, "my-lib"]),
        json!(["my-lib", { "from": "my-lib", "as": "a" }]),
        json!([{ "from": "my-lib", "as": "a" }, { "from": "my-lib", "as": "b" }]),
    ] {
        assert_eq!(
            ok(json!({ "importSources": order })).import_as("my-lib"),
            Some("a")
        );
    }

    // A built-in can be overridden.
    assert_eq!(
        ok(json!({ "importSources": [{ "from": "@stylexjs/stylex", "as": "css" }] }))
            .import_as("@stylexjs/stylex"),
        Some("css")
    );
}

#[test]
fn env_resolves_to_a_key_ordered_object() {
    assert_eq!(ok(json!({})).env, EvalValue::Obj(JsObjectMap::new().into()));
    assert_eq!(ok(json!({ "env": null })).env, ok(json!({})).env);
    assert_eq!(ok(json!({ "env": {} })).env, ok(json!({})).env);

    // `{...envInput}` iterates in ES OwnPropertyKeys order, which is what a
    // namespace-body env value expands to.
    let EvalValue::Obj(map) = ok(json!({
        "env": { "color": "red", "2": "b", "backgroundColor": "blue", "1": "a" }
    }))
    .env
    else {
        panic!("env is always an object");
    };
    let keys: Vec<&str> = map.entries().map(|(k, _)| k).collect();
    assert_eq!(keys, vec!["1", "2", "color", "backgroundColor"]);

    // Values are `any` at the option layer; downstream validators decide.
    let EvalValue::Obj(map) = ok(json!({
        "env": { "s": "x", "n": 18, "b": true, "z": null, "a": ["red"], "o": { "k": 1 } }
    }))
    .env
    else {
        panic!("env is always an object");
    };
    assert_eq!(map.get("n"), Some(&EvalValue::Num(18.0)));
    assert_eq!(map.get("b"), Some(&EvalValue::Bool(true)));
    assert_eq!(map.get("z"), Some(&EvalValue::Null));
    assert_eq!(map.get("missing"), None);
}

#[test]
fn supported_module_resolution() {
    let opts = ok(json!({
        "unstable_moduleResolution": { "type": "commonJS", "rootDir": "/test/" }
    }));
    assert_eq!(
        opts.unstable_module_resolution,
        Some(ModuleResolution {
            kind: ModuleResolutionType::CommonJs,
            root_dir: Some("/test/".into()),
            theme_file_extension: ".stylex".into(),
        })
    );
    let opts = ok(json!({
        "unstable_moduleResolution": {
            "type": "commonJS",
            "rootDir": "/app",
            "themeFileExtension": ".stylex"
        }
    }));
    assert_eq!(
        opts.unstable_module_resolution,
        Some(ModuleResolution {
            kind: ModuleResolutionType::CommonJs,
            root_dir: Some("/app".into()),
            theme_file_extension: ".stylex".into(),
        })
    );
    // No rootDir: upstream falls back to package-walk canonical names.
    let opts = ok(json!({ "unstable_moduleResolution": { "type": "commonJS" } }));
    assert_eq!(
        opts.unstable_module_resolution,
        Some(ModuleResolution {
            kind: ModuleResolutionType::CommonJs,
            root_dir: None,
            theme_file_extension: ".stylex".into(),
        })
    );
}

#[test]
fn theme_file_extension_is_taken_verbatim() {
    for ext in [".css", "cssvars", "", ".css.ts", "/vars"] {
        let opts = ok(json!({
            "unstable_moduleResolution": { "type": "commonJS", "themeFileExtension": ext }
        }));
        let resolution = opts.unstable_module_resolution.unwrap();
        assert_eq!(resolution.theme_file_extension, ext);
        assert_eq!(resolution.consts_file_extension(), format!("{ext}.const"));
    }
    // `??` collapses null and absent onto the default.
    for value in [json!(null), json!(".stylex")] {
        let opts = ok(json!({
            "unstable_moduleResolution": { "type": "commonJS", "themeFileExtension": value }
        }));
        assert_eq!(
            opts.unstable_module_resolution
                .unwrap()
                .theme_file_extension,
            ".stylex"
        );
    }
}

#[test]
fn haste_module_resolution_drops_root_dir() {
    for input in [
        json!({ "unstable_moduleResolution": { "type": "haste", "rootDir": "/app" } }),
        json!({ "unstable_moduleResolution": {
            "type": "haste", "rootDir": "/app", "themeFileExtension": ".stylex"
        } }),
    ] {
        let text = input.to_string();
        assert_eq!(
            ok(input).unstable_module_resolution,
            Some(ModuleResolution {
                kind: ModuleResolutionType::Haste,
                root_dir: None,
                theme_file_extension: ".stylex".into(),
            }),
            "input: {text}"
        );
    }
}

#[test]
fn hard_error_matrix() {
    let unsupported: Vec<Value> = vec![
        json!({ "unstable_moduleResolution": {
            "type": "custom", "filePathResolver": null, "getCanonicalFilePath": null
        } }),
        json!({ "unstable_moduleResolution": {
            "type": "experimental_crossFileParsing", "rootDir": "/test/"
        } }),
        json!({ "enableLTRRTLComments": true }),
        json!({ "debugFilePath": "some-transform" }),
        json!({ "include": ["src/**"] }),
        json!({ "exclude": ["node_modules/**"] }),
    ];
    for input in unsupported {
        let text = input.to_string();
        assert_eq!(
            err_code(input),
            ErrorCode::UnsupportedOption,
            "input: {text}"
        );
    }

    // The supported side of each matrix row.
    for input in [
        json!({ "styleResolution": "property-specificity" }),
        json!({ "styleResolution": "application-order" }),
        json!({ "styleResolution": "legacy-expand-shorthands" }),
        json!({ "enableLogicalStylesPolyfill": true }),
        json!({ "enableLegacyValueFlipping": true }),
        json!({ "aliases": null }),
        json!({ "aliases": { "@/*": ["./src/*"] } }),
        json!({ "rewriteAliases": false }),
        json!({ "rewriteAliases": true }),
        json!({ "enableLTRRTLComments": false }),
        json!({ "enableLogicalStylesPolyfill": false }),
        json!({ "enableLegacyValueFlipping": false }),
        json!({ "enableFontSizePxToRem": false }),
        json!({ "enableFontSizePxToRem": true }),
        json!({ "propertyValidationMode": "silent" }),
        json!({ "propertyValidationMode": "throw" }),
        json!({ "propertyValidationMode": "warn" }),
        json!({ "env": {} }),
        json!({ "env": { "FOO": "bar" } }),
        json!({ "unstable_moduleResolution": null }),
    ] {
        let text = input.to_string();
        assert!(resolve(input).is_ok(), "input: {text}");
    }
}

// Type-invalid values here are logAndDefault upstream, not a throw: they warn
// and compile with injection off, which the oracle probes pin exactly.
#[test]
fn runtime_injection_resolution_matrix() {
    let default_path = "@stylexjs/stylex/lib/stylex-inject";
    let on: Vec<(Value, &str, Option<&str>)> = vec![
        (json!({ "runtimeInjection": true }), default_path, None),
        (
            json!({ "runtimeInjection": "@custom/p" }),
            "@custom/p",
            None,
        ),
        (json!({ "runtimeInjection": "" }), "", None),
        (json!({ "runtimeInjection": "true" }), "true", None),
        (json!({ "runtimeInjection": "./rel.js" }), "./rel.js", None),
        (
            json!({ "runtimeInjection": { "from": "@custom/p", "as": "injectStyle" } }),
            "@custom/p",
            Some("injectStyle"),
        ),
        (
            json!({ "runtimeInjection": { "from": "p", "as": "i", "extra": 1 } }),
            "p",
            Some("i"),
        ),
    ];
    for (input, from, as_name) in on {
        let text = input.to_string();
        let injection = ok(input).runtime_injection.expect(&text);
        assert_eq!(injection.from, from, "input: {text}");
        assert_eq!(injection.as_name.as_deref(), as_name, "input: {text}");
    }

    for input in [
        json!({}),
        json!({ "runtimeInjection": false }),
        json!({ "runtimeInjection": null }),
        json!({ "runtimeInjection": 1 }),
        json!({ "runtimeInjection": ["a"] }),
        json!({ "runtimeInjection": { "from": "@custom/p" } }),
        json!({ "runtimeInjection": { "as": "injectStyle" } }),
        json!({ "runtimeInjection": { "from": 1, "as": "i" } }),
    ] {
        let text = input.to_string();
        assert_eq!(ok(input).runtime_injection, None, "input: {text}");
    }
}

#[test]
fn unknown_top_level_keys_are_rejected_by_name() {
    let err = resolve(json!({ "fooBar": true })).unwrap_err();
    assert_eq!(err.code, ErrorCode::UnknownOption);
    assert!(err.message.contains("fooBar"), "message: {}", err.message);

    let err = resolve(json!({ "useRemForFontSize": true })).unwrap_err();
    assert_eq!(err.code, ErrorCode::UnknownOption);
    assert!(err.message.contains("useRemForFontSize"));
}

// state-manager.js:444 hardcodes definedStylexCSSVariables to `{}` and its only
// reader `get cssVars()` is dead; useLayers belongs to processStylexRules.
#[test]
fn inert_upstream_keys_are_accepted_and_ignored() {
    for value in [
        json!({ "foo": 1 }),
        json!(null),
        json!("not-an-object"),
        json!([1, 2, 3]),
        json!(true),
        json!(0),
    ] {
        let input = json!({ "definedStylexCSSVariables": value });
        let text = input.to_string();
        assert_eq!(
            CompilerOptions::from_json(&input).expect(&text),
            CompilerOptions::default(),
            "input: {text}"
        );
    }

    for value in [
        json!(true),
        json!(false),
        json!(null),
        json!("wat"),
        json!({ "before": ["reset"], "after": ["z"], "prefix": "xds" }),
    ] {
        let input = json!({ "useLayers": value });
        let text = input.to_string();
        assert_eq!(
            CompilerOptions::from_json(&input).expect(&text),
            CompilerOptions::default(),
            "input: {text}"
        );
    }

    assert_eq!(
        ok(json!({
            "debug": true,
            "enableDebugClassNames": true,
            "useLayers": true,
            "definedStylexCSSVariables": { "foo": 1 },
        })),
        ok(json!({ "debug": true, "enableDebugClassNames": true })),
    );
}

// Deliberate divergence: upstream logAndDefault logs and falls back (to values that
// can differ from the ??-defaults, e.g. enableDebugClassNames→true); we hard-error.
#[test]
fn wrong_typed_values_hard_error() {
    let table: Vec<Value> = vec![
        json!({ "dev": "true" }),
        json!({ "debug": "true" }),
        json!({ "test": "true" }),
        json!({ "classNamePrefix": 1 }),
        json!({ "enableDebugClassNames": "false" }),
        json!({ "enableInlinedConditionalMerge": "false" }),
        json!({ "enableMinifiedKeys": "false" }),
        json!({ "importSources": 1 }),
        json!({ "importSources": [1] }),
        json!({ "importSources": [{ "from": "my-lib" }] }),
        json!({ "importSources": [{ "as": "css" }] }),
        json!({ "importSources": [{ "from": "my-lib", "as": 1 }] }),
        json!({ "importSources": [null] }),
        json!({ "importSources": [["my-lib"]] }),
        json!({ "styleResolution": "something-else" }),
        json!({ "propertyValidationMode": "something-else" }),
        json!({ "treeshakeCompensation": "true" }),
        json!({ "sxPropName": 1 }),
        json!({ "env": "production" }),
        json!("not-an-object"),
    ];
    for input in table {
        let text = input.to_string();
        assert_eq!(
            err_code(input),
            ErrorCode::InvalidOptionValue,
            "input: {text}"
        );
    }
}

#[test]
fn malformed_module_resolution_warns_and_defaults() {
    // parity: state-manager.js validates the whole option with
    // z.logAndDefault(..., null), so any malformation drops it entirely.
    for input in [
        json!({ "unstable_moduleResolution": "commonJS" }),
        json!({ "unstable_moduleResolution": { "type": "unknown" } }),
        json!({ "unstable_moduleResolution": { "type": "CommonJS" } }),
        json!({ "unstable_moduleResolution": { "type": 1 } }),
        json!({ "unstable_moduleResolution": { "type": "commonJS", "themeFileExtension": 1 } }),
    ] {
        let text = input.to_string();
        assert_eq!(ok(input).unstable_module_resolution, None, "input: {text}");
    }
    // An unrecognized key is dropped by the shape walk, not rejected.
    let kept = ok(json!({
        "unstable_moduleResolution": { "type": "haste", "bogus": 1 }
    }));
    assert_eq!(
        kept.unstable_module_resolution.map(|m| m.kind),
        Some(ModuleResolutionType::Haste)
    );
}

#[test]
fn error_messages_name_the_option() {
    let err = resolve(json!({ "enableLTRRTLComments": true })).unwrap_err();
    assert!(
        err.message.contains("enableLTRRTLComments"),
        "{}",
        err.message
    );
    assert!(
        err.message.contains("Rust StyleX compiler"),
        "{}",
        err.message
    );
    assert!(
        err.message.contains("crates/stylex/docs"),
        "{}",
        err.message
    );

    let err = resolve(json!({ "dev": "true" })).unwrap_err();
    assert_eq!(
        err.message,
        "Expected (options.dev) to be a boolean, but got `\"true\"`."
    );
    let err = resolve(json!({ "classNamePrefix": 1 })).unwrap_err();
    assert_eq!(
        err.message,
        "Expected (options.classNamePrefix) to be a string, but got `1`."
    );
}
