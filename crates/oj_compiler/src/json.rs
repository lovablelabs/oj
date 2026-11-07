// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use crate::CompileError;

fn named_keys(value: &serde_json::Value) -> Vec<String> {
    let Some(obj) = value.as_object() else {
        return Vec::new();
    };
    obj.keys()
        .filter(|k| is_safe_export_name(k))
        .cloned()
        .collect()
}

fn is_safe_export_name(name: &str) -> bool {
    // `__oj_json` is the module's own binding: a key with that exact name
    // would redeclare it. Reachable through the default export, like every
    // other skipped key.
    if name == "__proto__" || name == "__oj_json" {
        return false;
    }
    // Vite 8's JSON plugin is rolldown's: its named-export filter
    // (rolldown_utils::ecmascript::is_validate_assignee_identifier_name) is
    // oxc's reserved words + global values (Infinity, NaN, globalThis,
    // undefined) plus `arguments`/`eval`. pluginutils' wider list is the
    // rollup era; Vite 8 does export `Object`, `Math` and friends.
    if oxc_syntax::keyword::is_reserved_keyword_or_global_object(name)
        || name == "arguments"
        || name == "eval"
    {
        return false;
    }
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

fn parse(source: &str, url: &str) -> Result<serde_json::Value, CompileError> {
    serde_json::from_str(source).map_err(|e| CompileError::Parse {
        path: std::path::PathBuf::from(url),
        message: format!("invalid JSON: {e}"),
    })
}

fn has_proto_key(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            map.contains_key("__proto__") || map.values().any(has_proto_key)
        }
        serde_json::Value::Array(items) => items.iter().any(has_proto_key),
        _ => false,
    }
}

fn js_expression(source: &str, value: &serde_json::Value) -> String {
    let raw = source.trim();
    if has_proto_key(value) {
        return format!("JSON.parse({})", serde_json::Value::String(raw.to_string()));
    }
    raw.to_string()
}

pub fn to_esm(source: &str, url: &str) -> Result<String, CompileError> {
    let value = parse(source, url)?;
    let raw = js_expression(source, &value);
    let mut out = format!("const __oj_json = {raw};\nexport default __oj_json;\n");
    for key in named_keys(&value) {
        out.push_str(&format!("export const {key} = __oj_json[{key:?}];\n"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esm_default_and_named_exports() {
        let out = to_esm(r#"{"name":"oj","version":2,"is-kebab":1}"#, "/x.json").unwrap();
        assert!(out.contains("export default __oj_json"));
        assert!(
            out.contains(r#"export const name = __oj_json["name"]"#),
            "{out}"
        );
        assert!(out.contains("export const version ="), "{out}");
        assert!(
            !out.contains("is-kebab ="),
            "kebab key must be skipped: {out}"
        );
    }

    #[test]
    fn array_and_scalar_have_only_default() {
        let arr = to_esm("[1, 2, 3]", "/a.json").unwrap();
        assert!(arr.contains("const __oj_json = [1, 2, 3]"));
        assert_eq!(arr.matches("export ").count(), 1, "only default: {arr}");
    }

    #[test]
    fn reserved_key_default_is_skipped() {
        let out = to_esm(r#"{"default":1,"ok":2}"#, "/r.json").unwrap();
        assert!(!out.contains("export const default"), "{out}");
        assert!(out.contains("export const ok ="), "{out}");
    }

    #[test]
    fn invalid_json_errors() {
        assert!(to_esm("{ not json", "/bad.json").is_err());
    }

    #[test]
    fn filters_reserved_and_non_identifier_named_exports() {
        let out = to_esm(
            r#"{"class":1,"import":2,"await":3,"$ok":4,"_ok":5,"1bad":6,"a-b":7,"ok":8}"#,
            "/k.json",
        )
        .unwrap();
        assert_eq!(
            out.matches("export ").count(),
            4,
            "only default + 3 valid keys: {out}"
        );
        for ok in ["$ok", "_ok", "ok"] {
            assert!(
                out.contains(&format!("export const {ok} = __oj_json[")),
                "{ok} should export: {out}"
            );
        }
        // Vite 8's (rolldown's) filter: `arguments` binds nothing in strict
        // mode (the fuzz_json_module crash input); `eval`, reserved words and
        // the global values Infinity/NaN/globalThis/undefined are skipped too.
        // Plain globals (`Object`, `Math`) DO export, as under Vite 8, where
        // shadowing them in module scope is legal.
        let out = to_esm(
            r#"{"arguments":{},"eval":1,"static":2,"NaN":3,"globalThis":4,"Object":5,"Math":6,"kept":7}"#,
            "/r.json",
        )
        .unwrap();
        assert!(!out.contains("export const arguments"), "{out}");
        for skipped in ["eval", "static", "NaN", "globalThis"] {
            assert!(
                !out.contains(&format!("export const {skipped} ")),
                "{skipped} must not export: {out}"
            );
        }
        for kept in ["Object", "Math", "kept"] {
            assert!(
                out.contains(&format!("export const {kept} =")),
                "{kept} should export like Vite 8: {out}"
            );
        }
        // A key named like the module's own binding must not redeclare it
        // (the round-7 fuzz crash input).
        let out = to_esm(r#"{"__oj_json":{},"fine":1}"#, "/o.json").unwrap();
        assert!(!out.contains("export const __oj_json"), "{out}");
        assert!(out.contains("export const fine ="), "{out}");
    }

    #[test]
    fn nested_objects_and_scalars_export_only_default() {
        let nested = to_esm(r#"{"outer":{"inner":1}}"#, "/n.json").unwrap();
        assert!(nested.contains("export const outer ="), "{nested}");
        assert!(
            !nested.contains("export const inner ="),
            "nested keys must not leak: {nested}"
        );
        assert_eq!(
            nested.matches("export ").count(),
            2,
            "default + outer only: {nested}"
        );

        for (src, label) in [
            ("\"hello\"", "string"),
            ("42", "number"),
            ("true", "bool"),
            ("null", "null"),
        ] {
            let out = to_esm(src, "/s.json").unwrap();
            assert_eq!(
                out.matches("export ").count(),
                1,
                "{label} has only default: {out}"
            );
            assert!(out.contains("export default __oj_json"), "{label}: {out}");
        }
    }

    #[test]
    fn raw_json_formatting_is_preserved() {
        let src = "{\n  \"a\": 1,\n  \"b\": 2\n}";
        let out = to_esm(src, "/fmt.json").unwrap();
        assert!(out.contains(src), "raw formatting must be preserved: {out}");
    }
}
