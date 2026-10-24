//! `stylex.defaultMarker` / `stylex.defineMarker` compiled objects.
// parity: shared/stylex-defaultMarker.js + visitors/stylex-define-marker.js

use crate::eval::value::{EvalValue, JsObjectMap};
use crate::hash::hash;
use crate::options::ResolvedOptions;

/// `{prefix}-default-marker`; ResolvedOptions always carries a prefix, so the
/// upstream `classNamePrefix == null` no-dash branch is unreachable here.
pub fn default_marker_class_name(options: &ResolvedOptions) -> String {
    format!("{}-default-marker", options.class_name_prefix)
}

pub fn default_marker_object(options: &ResolvedOptions) -> JsObjectMap {
    self_map_object(default_marker_class_name(options))
}

pub fn define_marker_id(
    canonical_file_name: &str,
    export_name: &str,
    options: &ResolvedOptions,
) -> String {
    format!(
        "{}{}",
        options.class_name_prefix,
        hash(&format!("{canonical_file_name}//{export_name}"))
    )
}

/// `canonical_file_name` is state-manager fileNameForHashing output; callers
/// map a missing one to `cannot_generate_hash("defineMarker")` first.
pub fn define_marker_object(
    canonical_file_name: &str,
    export_name: &str,
    options: &ResolvedOptions,
) -> JsObjectMap {
    self_map_object(define_marker_id(canonical_file_name, export_name, options))
}

fn self_map_object(id: String) -> JsObjectMap {
    let mut obj = JsObjectMap::new();
    obj.insert(id.clone(), EvalValue::Str(id));
    obj.insert("$$css", EvalValue::Bool(true));
    obj
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oracle_pinned_objects() {
        // Pinned via live-oracle probe 2026-08-27.
        let options = ResolvedOptions::default();
        assert_eq!(
            default_marker_object(&options).to_json(),
            serde_json::json!({ "x-default-marker": "x-default-marker", "$$css": true })
        );
        let prefixed = ResolvedOptions {
            class_name_prefix: "pfx".to_string(),
            ..ResolvedOptions::default()
        };
        assert_eq!(default_marker_class_name(&prefixed), "pfx-default-marker");
        // tokens.stylex.ts under rootDir /fake/root, export name `marker`.
        assert_eq!(
            define_marker_object("tokens.stylex.ts", "marker", &options).to_json(),
            serde_json::json!({ "xleysvp": "xleysvp", "$$css": true })
        );
        assert_eq!(
            define_marker_id("tokens.stylex.ts", "marker", &prefixed),
            "pfxleysvp"
        );
    }
}
