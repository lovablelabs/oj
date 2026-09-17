// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Classification of stylesheets and components that need a JS-toolchain
//! compile (Tailwind/PostCSS, Less, Stylus, Svelte). These predicates gate the
//! lazy spawn of the in-process engines in [`crate::css_engine`]: a plain app
//! never boots one.

#[inline]
pub fn is_svelte(url: &str) -> bool {
    url.split('?').next().unwrap_or(url).ends_with(".svelte")
}

#[inline]
pub fn is_less(url: &str) -> bool {
    url.split('?').next().unwrap_or(url).ends_with(".less")
}

#[inline]
pub fn is_stylus(url: &str) -> bool {
    let f = url.split('?').next().unwrap_or(url);
    f.ends_with(".styl") || f.ends_with(".stylus")
}

pub fn is_tailwind_css(source: &str) -> bool {
    for (index, _) in source.match_indices('@') {
        let rest = &source[index..];
        if !at_directive_position(source, index) {
            continue;
        }
        if let Some(after) = rest.strip_prefix("@import") {
            let target = after.trim_start().trim_start_matches(['"', '\'']);
            if let Some(tail) = target.strip_prefix("tailwindcss") {
                if tail
                    .chars()
                    .next()
                    .is_none_or(|c| matches!(c, '"' | '\'' | '/') || c.is_whitespace())
                {
                    return true;
                }
            }
            continue;
        }
        for directive in ["@tailwind", "@theme", "@utility", "@apply", "@source"] {
            if let Some(after) = rest.strip_prefix(directive) {
                if after
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric() && c != '-' && c != '_')
                {
                    return true;
                }
            }
        }
    }
    false
}

fn at_directive_position(source: &str, index: usize) -> bool {
    source[..index]
        .chars()
        .rev()
        .find(|c| !matches!(c, ' ' | '\t'))
        .is_none_or(|c| matches!(c, '\n' | '\r' | '}' | '{' | ';'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailwind_imports_are_detected_in_every_written_form() {
        for source in [
            "@import \"tailwindcss\";",
            "@import 'tailwindcss';",
            "@import \"tailwindcss\"",
            "  @import \"tailwindcss\";",
            "\n\n@import \"tailwindcss\";\n",
            // v4 subpath and layer forms.
            "@import \"tailwindcss/preflight\" layer(base);",
            "@import \"tailwindcss/theme\" layer(theme);",
            "@import \"tailwindcss/utilities\" layer(utilities);",
            // Other statements around it.
            "@charset \"utf-8\";\n@import \"tailwindcss\";",
            ".a { color: red }\n@import \"tailwindcss\";",
        ] {
            assert!(is_tailwind_css(source), "missed: {source:?}");
        }
    }

    #[test]
    fn tailwind_directives_are_detected_only_where_a_directive_can_start() {
        for source in [
            "@tailwind base;",
            "@tailwind utilities;",
            "@theme { --color-brand: red }",
            "@theme inline { --x: 1 }",
            ".btn {\n  @apply underline;\n}",
            "@utility tab-4 { tab-size: 4 }",
            "@source \"./src/**/*.tsx\";",
            "}\n@theme { --x: 1 }",
        ] {
            assert!(is_tailwind_css(source), "missed: {source:?}");
        }
    }

    #[test]
    fn a_plain_stylesheet_never_reaches_the_tailwind_engine() {
        // A false positive here fails the build with "is tailwindcss installed?"
        // on a stylesheet that has nothing to do with Tailwind.
        for source in [
            "",
            ".a { color: red }",
            "@media (min-width: 1px) { .a { color: red } }",
            "@supports (display: grid) { .a { display: grid } }",
            "@font-face { font-family: X; src: url(x.woff2) }",
            "@keyframes spin { to { transform: rotate(1turn) } }",
            "@layer base { .a { color: red } }",
            "@import \"./other.css\";",
            "@import \"@acme/design/tokens.css\";",
            // The word appears, but not as a directive.
            "/* @theme is not used here */",
            "/* @tailwind base; */",
            ".a { content: \"@theme\" }",
            ".a { content: \"@import \\\"tailwindcss\\\"\" }",
            "@themes { --x: 1 }",
            "@theming { --x: 1 }",
            ".a[data-x=\"@apply\"] { color: red }",
            "/* see @source for details */",
            "@import \"tailwindcss-is-not-this-package\";",
        ] {
            assert!(!is_tailwind_css(source), "false positive: {source:?}");
        }
    }

    #[test]
    fn an_import_of_a_package_named_after_tailwind_is_not_tailwind() {
        // The prefix has to end at a boundary the package itself uses.
        assert!(is_tailwind_css("@import \"tailwindcss\";"));
        assert!(is_tailwind_css("@import \"tailwindcss/theme\";"));
        assert!(!is_tailwind_css("@import \"tailwindcss-animate\";"));
        assert!(!is_tailwind_css("@import \"my-tailwindcss\";"));
    }

    #[test]
    fn style_dialects_are_classified_by_extension_not_by_query() {
        assert!(is_less("/src/a.less"));
        assert!(is_less("/src/a.less?inline"));
        assert!(!is_less("/src/a.less/b.css"));
        assert!(!is_less("/src/a.css?x=.less"));

        assert!(is_stylus("/src/a.styl"));
        assert!(is_stylus("/src/a.stylus"));
        assert!(is_stylus("/src/a.styl?raw"));
        assert!(!is_stylus("/src/a.style"));
        assert!(!is_stylus("/src/a.css?x=.styl"));

        assert!(is_svelte("/src/A.svelte"));
        assert!(is_svelte("/src/A.svelte?raw"));
        assert!(!is_svelte("/src/A.svelte.ts"));
        assert!(!is_svelte("/src/a.ts?x=.svelte"));

        for predicate in [is_less, is_stylus, is_svelte] {
            assert!(!predicate(""));
            assert!(!predicate("?"));
            assert!(!predicate("/"));
        }
    }
}
