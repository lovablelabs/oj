//! Vite's default resolve lists, shared with the dependency optimizer and the
//! build so every consumer resolves alike.

/// Vite's DEFAULT_EXTENSIONS, in probe order: `./foo` with both `foo.js` and
/// `foo.ts` picks `foo.js`.
pub fn default_extensions() -> Vec<String> {
    [".mjs", ".js", ".mts", ".ts", ".jsx", ".tsx", ".json"]
        .map(String::from)
        .to_vec()
}

/// Entry fields for deps without `exports`. `module` leads so a dep shipping an
/// ESM build next to a `browser` string pointing at a UMD/CJS bundle serves its
/// ESM. The `browser` object remap runs through alias fields, not here.
pub fn default_main_fields() -> Vec<String> {
    ["module", "browser", "jsnext:main", "jsnext", "main"]
        .map(String::from)
        .to_vec()
}

/// Vite's DEFAULT_SERVER_MAIN_FIELDS: the client list without `browser`.
pub fn default_server_main_fields() -> Vec<String> {
    default_main_fields()
        .into_iter()
        .filter(|f| f != "browser")
        .collect()
}

/// Vite's `pkg.main` fallback as a list transform: resolvePackageEntry always
/// falls back to `main` after the mainFields walk. Resolvers without that
/// fallback (oxc_resolver, rolldown, the optimizer) take the list with `main`
/// appended last, so it never outranks the configured fields.
pub fn with_main_fallback(mut fields: Vec<String>) -> Vec<String> {
    if !fields.iter().any(|f| f == "main") {
        fields.push("main".to_string());
    }
    fields
}

/// The export conditions as rolldown takes them: without the ones its resolver
/// adds per import kind (`import` for an import, `require` for a `require()`,
/// `default` for both). Vite's getConditions pushes `import` or `require` by
/// the importer's kind; a list that already carries `import` would hand a
/// `require()` the ESM build of a dual package, and the CJS caller gets a
/// namespace where it expects the module's value.
pub fn rolldown_conditions(conditions: Vec<String>) -> Vec<String> {
    conditions
        .into_iter()
        .filter(|c| !matches!(c.as_str(), "import" | "require" | "default"))
        .collect()
}

/// Vite's TS-output remap (`isPossibleTsOutput`): `./x.js` with no such file
/// resolves to its TypeScript source. The real extension leads, so an existing
/// `.js` wins over a sibling `.ts`. Applies to every filesystem path.
pub fn default_extension_alias() -> Vec<(String, Vec<String>)> {
    [
        (".js", &[".js", ".ts", ".tsx"][..]),
        (".jsx", &[".jsx", ".tsx"][..]),
        (".mjs", &[".mjs", ".mts"][..]),
        (".cjs", &[".cjs", ".cts"][..]),
    ]
    .iter()
    .map(|(ext, alts)| {
        (
            ext.to_string(),
            alts.iter().map(|s| s.to_string()).collect(),
        )
    })
    .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn rolldown_conditions_leave_the_per_kind_ones_to_rolldown() {
        let list = [
            "browser",
            "import",
            "module",
            "development",
            "default",
            "require",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            super::rolldown_conditions(list),
            ["browser", "module", "development"]
                .map(String::from)
                .to_vec()
        );
    }

    use super::*;

    #[test]
    fn with_main_fallback_appends_main_last_and_dedups() {
        assert_eq!(
            with_main_fallback(["browser", "module"].map(String::from).to_vec()),
            ["browser", "module", "main"].map(String::from)
        );
        // Already present anywhere: untouched, never outranked or duplicated.
        assert_eq!(
            with_main_fallback(["main", "module"].map(String::from).to_vec()),
            ["main", "module"].map(String::from)
        );
        assert_eq!(with_main_fallback(Vec::new()), ["main".to_string()]);
    }
}
