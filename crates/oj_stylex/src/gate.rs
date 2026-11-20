//! The pre-parse gate: whether a source can reference StyleX at all, decided
//! by SIMD substring scans whose searchers are built once per option set, and
//! those of the default needles once per process.

use std::sync::LazyLock;

use memchr::memmem::Finder;

use crate::imports::ATOMS_SOURCE;
use crate::module_resolution::THEME_FILE_EXTENSION;
use crate::options::{DEFAULT_SX_PROP_NAME, ImportSource};

/// The built-in import source every other built-in needle contains.
const STYLEX: &str = "stylex";

// The searchers of the default needles, one set for the process, built when
// the first source is scanned: an option set that names no needle of its own
// only points at them, so resolving it builds and allocates nothing here.
static DEFAULT_SX: LazyLock<Finder<'static>> =
    LazyLock::new(|| Finder::new(DEFAULT_SX_PROP_NAME.as_bytes()));
static STYLEX_ALONE: LazyLock<[Finder<'static>; 1]> =
    LazyLock::new(|| [Finder::new(STYLEX.as_bytes())]);

#[derive(Clone)]
pub(crate) struct PreGate {
    /// The sx prop compiles with no stylex import in the file at all.
    sx: Option<SxSearcher>,
    sources: SourceSearchers,
}

#[derive(Clone)]
enum SxSearcher {
    /// [`DEFAULT_SX`].
    Default,
    /// Boxed: the options of the default name carry no searcher around.
    Named(Box<Finder<'static>>),
}

#[derive(Clone)]
enum SourceSearchers {
    /// [`STYLEX_ALONE`].
    Stylex,
    Listed(Vec<Finder<'static>>),
}

impl PreGate {
    pub(crate) fn new(
        sx_prop_name: Option<&str>,
        import_sources: &[ImportSource],
        rewrite_aliases: bool,
    ) -> Self {
        let sx = sx_prop_name.map(|name| match name {
            DEFAULT_SX_PROP_NAME => SxSearcher::Default,
            name => SxSearcher::Named(Box::new(searcher(name))),
        });
        let sources = || import_sources.iter().map(ImportSource::from_specifier);
        // A needle containing another (or repeating an earlier one) can only
        // hit where that one hits, and both hardcoded needles below contain
        // "stylex". Where that is an import source and every other source
        // contains it too, "stylex" alone is left: the one searcher all such
        // sets share. (The built-in sources end in it, which is settled
        // without setting up a substring search.)
        if sources().any(|source| source == STYLEX)
            && sources().all(|source| source.ends_with(STYLEX) || source.contains(STYLEX))
        {
            return Self {
                sx,
                sources: SourceSearchers::Stylex,
            };
        }
        // Atoms compile off a hardcoded source no importSources setting
        // covers; a rewritable import source must carry the hardcoded
        // `.stylex` suffix.
        let needles: Vec<&str> = sources()
            .chain(std::iter::once(ATOMS_SOURCE))
            .chain(rewrite_aliases.then_some(THEME_FILE_EXTENSION))
            .collect();
        let sources = needles
            .iter()
            .enumerate()
            .filter(|&(i, needle)| {
                !needles.iter().enumerate().any(|(j, other)| {
                    (other.len() < needle.len() && needle.contains(other))
                        || (j < i && other == needle)
                })
            })
            .map(|(_, needle)| searcher(needle))
            .collect();
        Self {
            sx,
            sources: SourceSearchers::Listed(sources),
        }
    }

    fn sx(&self) -> Option<&Finder<'static>> {
        self.sx.as_ref().map(|sx| match sx {
            SxSearcher::Default => &*DEFAULT_SX,
            SxSearcher::Named(searcher) => searcher,
        })
    }

    fn sources(&self) -> &[Finder<'static>] {
        match &self.sources {
            SourceSearchers::Stylex => &*STYLEX_ALONE,
            SourceSearchers::Listed(searchers) => searchers,
        }
    }

    /// `None` = the file cannot reference any stylex import source; `Some(sx)`
    /// carries whether the sx-prop needle hit, sparing the dormancy check a
    /// rescan.
    pub(crate) fn scan(&self, source: &str) -> Option<bool> {
        let bytes = source.as_bytes();
        if self.sx().is_some_and(|sx| sx.find(bytes).is_some()) {
            return Some(true);
        }
        let hit = self.sources().iter().any(|s| s.find(bytes).is_some());
        // A `\u`/`\x` escape can cook into an import-source match the raw
        // needles miss (`"@stylexjs/stylex"`); parse and decide post-parse.
        let escapes = || {
            memchr::memchr_iter(b'\\', bytes).any(|i| matches!(bytes.get(i + 1), Some(b'u' | b'x')))
        };
        (hit || escapes()).then_some(false)
    }

    /// Where the sx prop's name occurs in `source`, in order.
    pub(crate) fn sx_offsets<'s>(&'s self, source: &'s str) -> impl Iterator<Item = usize> + 's {
        self.sx()
            .into_iter()
            .flat_map(|sx| sx.find_iter(source.as_bytes()))
    }
}

fn searcher(needle: &str) -> Finder<'static> {
    Finder::new(needle.as_bytes()).into_owned()
}

// Derived from the options it sits in, so it neither distinguishes two option
// sets nor adds to their `cache_repr` (a searcher's own Debug names the SIMD
// kernel picked for this CPU).
impl PartialEq for PreGate {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for PreGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreGate")
    }
}

#[cfg(test)]
mod tests {
    use memchr::memmem;

    use super::*;
    use crate::options::{CompilerOptions, ResolvedOptions};

    fn options(json: serde_json::Value) -> ResolvedOptions {
        CompilerOptions::from_json(&json)
            .unwrap()
            .resolve()
            .unwrap()
    }

    fn pre_gate(source: &str, options: &ResolvedOptions) -> Option<bool> {
        options.gate.scan(source)
    }

    /// The gate as specified, one scan per needle and nothing prebuilt.
    fn reference(source: &str, options: &ResolvedOptions) -> Option<bool> {
        let bytes = source.as_bytes();
        let has = |needle: &str| memmem::find(bytes, needle.as_bytes()).is_some();
        if options.sx_prop_name.as_deref().is_some_and(has) {
            return Some(true);
        }
        let hit = options
            .import_sources
            .iter()
            .map(ImportSource::from_specifier)
            .chain([ATOMS_SOURCE])
            .chain(options.rewrite_aliases.then_some(THEME_FILE_EXTENSION))
            .any(has);
        let escapes = bytes
            .windows(2)
            .any(|pair| pair[0] == b'\\' && matches!(pair[1], b'u' | b'x'));
        (hit || escapes).then_some(false)
    }

    #[test]
    fn pre_gate_admits_every_needle_and_reports_sx() {
        let opts = ResolvedOptions::default();
        assert_eq!(pre_gate("export const x = 1;\n", &opts), None);
        for (source, sx) in [
            ("import * as s from '@stylexjs/stylex';", false),
            ("import s from 'stylex';", false),
            ("import { color } from '@stylexjs/atoms';", false),
            ("const t = './tokens.stylex';", false),
            ("<div sx={x} />", true),
            ("const tsx = 1;", true),
            ("const s = '\\u0040stylexjs/stylex';", false),
            ("const s = '\\x40';", false),
        ] {
            assert_eq!(pre_gate(source, &opts), Some(sx), "{source}");
        }
    }

    #[test]
    fn pre_gate_keeps_non_default_needles_and_honours_sx_off() {
        let opts = options(serde_json::json!({
            "importSources": ["foo-bar", { "from": "my-stylex-lib", "as": "css" }],
            "sxPropName": false,
        }));
        assert_eq!(
            pre_gate("import { css } from 'foo-bar';", &opts),
            Some(false)
        );
        assert_eq!(
            pre_gate("import { css } from 'my-stylex-lib';", &opts),
            Some(false)
        );
        assert_eq!(pre_gate("<div sx={x} />", &opts), None);
        let custom_sx = options(serde_json::json!({ "sxPropName": "css" }));
        assert_eq!(pre_gate("<div css={x} />", &custom_sx), Some(true));
        assert_eq!(pre_gate("<div sx={x} />", &custom_sx), None);
    }

    #[test]
    fn default_sources_collapse_to_one_scan() {
        assert_eq!(ResolvedOptions::default().gate.sources().len(), 1);
        // Neither custom source contains another needle; the built-ins and
        // the atoms source still ride on "stylex".
        let custom = options(serde_json::json!({
            "importSources": ["@acme/ui", { "from": "@acme/ui/css", "as": "css" }],
        }));
        assert_eq!(custom.gate.sources().len(), 2);
        let repeated = options(serde_json::json!({ "importSources": ["stylex", "x"] }));
        assert_eq!(repeated.gate.sources().len(), 1);
    }

    #[test]
    fn default_needles_share_the_searchers_built_once() {
        // What lets `PreGate::new` leave the hardcoded needles unread.
        assert!(ATOMS_SOURCE.contains(STYLEX) && THEME_FILE_EXTENSION.contains(STYLEX));
        // (the sx searcher, the source searchers): the shared one, or none.
        let shared = |json: serde_json::Value| {
            let gate = options(json).gate;
            (
                matches!(gate.sx, None | Some(SxSearcher::Default)),
                matches!(gate.sources, SourceSearchers::Stylex),
            )
        };
        for json in [
            serde_json::json!({}),
            serde_json::json!({ "dev": true, "sxPropName": false }),
            serde_json::json!({ "rewriteAliases": true, "sxPropName": "sx" }),
            serde_json::json!({
                "importSources": ["@stylexjs/stylex", { "from": "my-stylex-lib", "as": "css" }],
            }),
        ] {
            assert_eq!(shared(json.clone()), (true, true), "{json}");
        }
        // A needle of its own is the only one an option set builds.
        assert_eq!(
            shared(serde_json::json!({ "sxPropName": "css" })),
            (false, true)
        );
        assert_eq!(
            shared(serde_json::json!({ "importSources": ["foo-bar"] })),
            (true, false)
        );
    }

    #[test]
    fn the_gate_stays_out_of_the_options_identity() {
        let a = options(serde_json::json!({ "importSources": ["a"] }));
        let b = options(serde_json::json!({ "importSources": ["b"] }));
        assert_ne!(a, b);
        assert_ne!(a.cache_repr, b.cache_repr);
        assert_eq!(format!("{:?}", a.gate), format!("{:?}", b.gate));
    }

    #[test]
    fn prebuilt_gate_matches_the_per_needle_scan() {
        let option_sets = [
            serde_json::json!({}),
            serde_json::json!({ "sxPropName": false }),
            serde_json::json!({ "sxPropName": "css" }),
            serde_json::json!({ "rewriteAliases": true, "sxPropName": false }),
            serde_json::json!({ "importSources": ["x", "lex", "@acme/ui"], "sxPropName": false }),
            serde_json::json!({
                "importSources": ["@acme/ui", { "from": "@acme/ui/css", "as": "css" }, "@acme/ui"],
                "sxPropName": "styl",
            }),
            serde_json::json!({ "importSources": ["./local.stylex", "st"], "rewriteAliases": true }),
        ];
        // Fragments that assemble into every needle, its near misses, and
        // the escapes, glued by a fixed-seed generator.
        let fragments = [
            "s",
            "x",
            "sx",
            "st",
            "styl",
            "style",
            "stylex",
            "lex",
            "@",
            "@stylexjs/",
            "atoms",
            "@acme/",
            "ui",
            "/css",
            "css",
            ".",
            ".style",
            "./local",
            "\\",
            "\\u",
            "\\x",
            "u0040",
            " ",
            "\n",
            "'",
            "from ",
            "import ",
            "é",
            "const a = 1;",
        ];
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let resolved: Vec<ResolvedOptions> = option_sets.into_iter().map(options).collect();
        let (mut none, mut import, mut sx) = (0, 0, 0);
        for _ in 0..4000 {
            let len = next() % 7;
            let source: String = (0..len)
                .map(|_| fragments[(next() % fragments.len() as u64) as usize])
                .collect();
            for opts in &resolved {
                let got = pre_gate(&source, opts);
                assert_eq!(got, reference(&source, opts), "{source:?} under {opts:?}");
                match got {
                    None => none += 1,
                    Some(false) => import += 1,
                    Some(true) => sx += 1,
                }
            }
        }
        assert!(
            none > 100 && import > 100 && sx > 100,
            "{none} {import} {sx}"
        );
    }
}
