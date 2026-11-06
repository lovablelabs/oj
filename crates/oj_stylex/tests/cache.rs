//! Cache correctness: the invalidation taxonomy, verify-mode poison
//! detection, and the seeded fs-mutation fuzzer (cached == cold, every step).

use std::path::PathBuf;

use oj_stylex::api::{FileContext, SourceCompileResult, transform_source};
use oj_stylex::cache::{
    CacheStatus, CacheStore, cache_key, cache_key_with_fingerprint, compare_results,
    compile_through_cache, filename_key,
};
use oj_stylex::module_resolution::{BoundedFs, ResolveConfig};
use oj_stylex::options::{CompilerOptions, ResolvedOptions};

const SOURCE: &str = "import * as stylex from '@stylexjs/stylex';\n\
import { breakpoints } from './tokens.stylex';\n\
import { palette } from 'design-tokens/theme.stylex';\n\
export const styles = stylex.create({ a: { color: { default: palette.accent, [breakpoints.md]: 'red' } } });\n";

const TOKENS: &str = "import * as stylex from '@stylexjs/stylex';\n\
export const breakpoints = stylex.defineConsts({ md: '@media (min-width: 768px)' });\n";

struct Fixture {
    root: PathBuf,
    store_dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("cache-{name}"));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("proj");
        let fx = Fixture {
            root,
            store_dir: base.join("store"),
        };
        fx.write("package.json", "{ \"name\": \"app-pkg\" }\n");
        fx.write("README.md", "readme\n");
        fx.write("src/other.ts", "export const other = 1;\n");
        fx.write("src/tokens.stylex.ts", TOKENS);
        fx.write(
            "node_modules/design-tokens/package.json",
            &dt_package_json("./out/theme.stylex.js", None),
        );
        fx.write("node_modules/design-tokens/out/theme.stylex.js", "");
        fx.write("node_modules/design-tokens/out/theme2.stylex.js", "");
        fx
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn remove(&self, rel: &str) {
        let _ = std::fs::remove_file(self.root.join(rel));
    }

    fn options(&self, dev: bool) -> ResolvedOptions {
        let json = serde_json::json!({
            "dev": dev,
            "unstable_moduleResolution": {
                "type": "commonJS",
                "rootDir": self.root.to_string_lossy(),
            },
        });
        CompilerOptions::from_json(&json)
            .expect("options parse")
            .resolve()
            .expect("options resolve")
    }

    fn fs(&self) -> BoundedFs {
        BoundedFs {
            root: self.root.clone(),
        }
    }

    fn compile_cold(
        &self,
        source: &str,
        opts: &ResolvedOptions,
    ) -> Result<Option<SourceCompileResult>, oj_stylex::errors::StylexError> {
        let filename = self.root.join("src/input.ts");
        let ctx = FileContext {
            filename: &filename,
            source_text: source,
            cwd: &self.root,
        };
        transform_source(&ctx, opts, &self.fs())
    }

    fn compile_cached(
        &self,
        store: &CacheStore,
        source: &str,
        opts: &ResolvedOptions,
        verify: bool,
    ) -> Result<(Option<SourceCompileResult>, CacheStatus), oj_stylex::errors::StylexError> {
        let filename = self.root.join("src/input.ts");
        let ctx = FileContext {
            filename: &filename,
            source_text: source,
            cwd: &self.root,
        };
        compile_through_cache(store, &ctx, opts, &self.fs(), verify)
    }
}

fn dt_package_json(theme_target: &str, extra_field: Option<&str>) -> String {
    let extra = extra_field.map_or(String::new(), |v| format!(",\"note\":\"{v}\""));
    format!(
        "{{\"name\":\"design-tokens\",\"exports\":{{\"./theme.stylex\":\"{theme_target}\"}}{extra}}}"
    )
}

fn assert_status_hit(status: &CacheStatus) {
    assert!(
        matches!(status, CacheStatus::Hit),
        "expected hit: {status:?}"
    );
}

fn assert_status_miss(status: &CacheStatus) {
    assert!(
        matches!(status, CacheStatus::Miss),
        "expected miss: {status:?}"
    );
}

#[test]
fn dep_mutations_miss_and_unrelated_mutations_hit() {
    let fx = Fixture::new("mutations");
    let opts = fx.options(false);
    let store = CacheStore::new(&fx.store_dir).unwrap();
    let key = cache_key(
        SOURCE,
        &filename_key(&fx.root, &fx.root.join("src/input.ts")),
        &opts,
    );
    let lookup = || store.get_validated(&key, &fx.root, &fx.fs(), ResolveConfig::of(&opts));
    // Reprime after a dep mutation is reverted: the entry then holds the
    // mutated tree's dep log, so the first compile after revert misses.
    let reprime = |expect_same_as: &SourceCompileResult| {
        let (result, status) = fx.compile_cached(&store, SOURCE, &opts, false).unwrap();
        assert_status_miss(&status);
        compare_results(&result.unwrap(), expect_same_as).unwrap();
        let (result, status) = fx.compile_cached(&store, SOURCE, &opts, false).unwrap();
        assert_status_hit(&status);
        compare_results(&result.unwrap(), expect_same_as).unwrap();
    };

    let (first, status) = fx.compile_cached(&store, SOURCE, &opts, false).unwrap();
    assert_status_miss(&status);
    let first = first.unwrap();
    let (again, status) = fx.compile_cached(&store, SOURCE, &opts, false).unwrap();
    assert_status_hit(&status);
    compare_results(&again.unwrap(), &first).unwrap();

    // An earlier-probe file appearing re-resolves ./tokens.stylex (.js wins
    // over .ts) — must miss, and the recompiled output must actually differ.
    fx.write("src/tokens.stylex.js", TOKENS);
    assert!(lookup().is_none(), "earlier-probe file must invalidate");
    let (shifted, status) = fx.compile_cached(&store, SOURCE, &opts, false).unwrap();
    assert_status_miss(&status);
    assert_ne!(shifted.unwrap().code, first.code);
    fx.remove("src/tokens.stylex.js");
    reprime(&first);

    // Theme rename changes the resolved path — must miss; reverting restores
    // the tree, so the untouched entry re-validates (replay, not timestamps).
    std::fs::rename(
        fx.root.join("src/tokens.stylex.ts"),
        fx.root.join("src/tokens.stylex.tsx"),
    )
    .unwrap();
    assert!(lookup().is_none(), "theme rename must invalidate");
    std::fs::rename(
        fx.root.join("src/tokens.stylex.tsx"),
        fx.root.join("src/tokens.stylex.ts"),
    )
    .unwrap();
    compare_results(&lookup().expect("revert re-validates"), &first).unwrap();

    // App package.json name edit changes canonical theme names — must miss.
    fx.write("package.json", "{ \"name\": \"app-pkg-renamed\" }\n");
    assert!(lookup().is_none(), "package name edit must invalidate");
    fx.write("package.json", "{ \"name\": \"app-pkg\" }\n");
    compare_results(&lookup().expect("revert re-validates"), &first).unwrap();

    // Exports retarget re-resolves the package theme import — must miss.
    fx.write(
        "node_modules/design-tokens/package.json",
        &dt_package_json("./out/theme2.stylex.js", None),
    );
    assert!(lookup().is_none(), "exports retarget must invalidate");
    fx.write(
        "node_modules/design-tokens/package.json",
        &dt_package_json("./out/theme.stylex.js", None),
    );
    compare_results(&lookup().expect("revert re-validates"), &first).unwrap();

    // A cosmetic package.json edit misses conservatively (content hash) but
    // recompiles to identical output.
    fx.write(
        "node_modules/design-tokens/package.json",
        &dt_package_json("./out/theme.stylex.js", Some("cosmetic")),
    );
    assert!(lookup().is_none(), "package.json content hash is exact");
    let (result, status) = fx.compile_cached(&store, SOURCE, &opts, false).unwrap();
    assert_status_miss(&status);
    compare_results(&result.unwrap(), &first).unwrap();

    // Unrelated mutations must all still hit: sibling files, and theme file
    // CONTENT (cross-module resolution is path identity, never file reads).
    fx.write("README.md", "changed\n");
    fx.write("src/other.ts", "export const other = 2;\n");
    fx.write("src/tokens.stylex.ts", &format!("{TOKENS}// trailing\n"));
    let hit = lookup().expect("unrelated mutations must not invalidate");
    compare_results(&hit, &first).unwrap();

    // Source, options, and compiler-version changes re-key.
    let file_key = filename_key(&fx.root, &fx.root.join("src/input.ts"));
    assert_ne!(key, cache_key(&format!("{SOURCE} "), &file_key, &opts));
    assert_ne!(key, cache_key(SOURCE, &file_key, &fx.options(true)));
    assert_ne!(
        key,
        cache_key_with_fingerprint(SOURCE, &file_key, &opts, "next-version")
    );
}

#[test]
fn verify_mode_detects_a_poisoned_entry() {
    let fx = Fixture::new("verify");
    let opts = fx.options(false);
    let store = CacheStore::new(&fx.store_dir).unwrap();
    let (fresh, status) = fx.compile_cached(&store, SOURCE, &opts, false).unwrap();
    assert_status_miss(&status);
    let fresh = fresh.unwrap();

    let key = cache_key(
        SOURCE,
        &filename_key(&fx.root, &fx.root.join("src/input.ts")),
        &opts,
    );
    let entry_path = store.dir().join(key.hex());
    let mut entry: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&entry_path).unwrap()).unwrap();
    entry["result"]["code"] = serde_json::Value::String("POISON".into());
    std::fs::write(&entry_path, serde_json::to_vec(&entry).unwrap()).unwrap();

    let poisoned = store
        .get_validated(&key, &fx.root, &fx.fs(), ResolveConfig::of(&opts))
        .expect("deps still validate");
    assert_eq!(poisoned.code, "POISON");

    let (result, status) = fx.compile_cached(&store, SOURCE, &opts, true).unwrap();
    match status {
        CacheStatus::Poisoned(report) => assert_eq!(report.field, "code"),
        other => panic!("expected Poisoned, got {other:?}"),
    }
    compare_results(&result.unwrap(), &fresh).unwrap();

    // The poisoned entry was repaired in place.
    let (repaired, status) = fx.compile_cached(&store, SOURCE, &opts, false).unwrap();
    assert_status_hit(&status);
    compare_results(&repaired.unwrap(), &fresh).unwrap();
}

#[test]
fn non_stylex_sources_gate_without_touching_the_store() {
    let fx = Fixture::new("gate");
    let opts = fx.options(false);
    let store = CacheStore::new(&fx.store_dir).unwrap();
    let (result, status) = fx
        .compile_cached(&store, "export const a = 1;\n", &opts, false)
        .unwrap();
    assert!(result.is_none());
    assert!(matches!(status, CacheStatus::Gated));
    assert_eq!(std::fs::read_dir(store.dir()).unwrap().count(), 0);
}

// ------------------------------------------------------ invalidation fuzzer

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn pick(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Clone, Debug)]
struct FuzzState {
    source_rev: u32,
    dev: bool,
    tokens_ext: &'static str,
    early_probe: bool,
    app_pkg_rev: u32,
    exports_theme2: bool,
    dt_pkg_present: bool,
    theme_content_rev: u32,
    readme_rev: u32,
}

impl FuzzState {
    fn source(&self) -> String {
        format!("{SOURCE}// rev {}\n", self.source_rev)
    }

    fn sync(&self, fx: &Fixture) {
        fx.write(
            "package.json",
            &format!("{{ \"name\": \"app-pkg-{}\" }}\n", self.app_pkg_rev),
        );
        fx.write("README.md", &format!("readme {}\n", self.readme_rev));
        let tokens = format!("{TOKENS}// content rev {}\n", self.theme_content_rev);
        for ext in ["ts", "tsx"] {
            fx.remove(&format!("src/tokens.stylex.{ext}"));
        }
        fx.write(&format!("src/tokens.stylex.{}", self.tokens_ext), &tokens);
        if self.early_probe {
            fx.write("src/tokens.stylex.js", &tokens);
        } else {
            fx.remove("src/tokens.stylex.js");
        }
        let target = if self.exports_theme2 {
            "./out/theme2.stylex.js"
        } else {
            "./out/theme.stylex.js"
        };
        if self.dt_pkg_present {
            fx.write(
                "node_modules/design-tokens/package.json",
                &dt_package_json(target, None),
            );
        } else {
            fx.remove("node_modules/design-tokens/package.json");
        }
    }
}

/// Random fs mutations, then cold + cache-mediated compiles that must agree;
/// STYLEX_CACHE_FUZZ_ITERS scales it up (acceptance: 500).
#[test]
fn fuzz_invalidation() {
    let iters: u32 = std::env::var("STYLEX_CACHE_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let seed: u64 = std::env::var("STYLEX_CACHE_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5EED_CAFE);
    let fx = Fixture::new("fuzz");
    let store = CacheStore::new(&fx.store_dir).unwrap();
    let mut rng = SplitMix64(seed);
    let mut st = FuzzState {
        source_rev: 0,
        dev: false,
        tokens_ext: "ts",
        early_probe: false,
        app_pkg_rev: 0,
        exports_theme2: false,
        dt_pkg_present: true,
        theme_content_rev: 0,
        readme_rev: 0,
    };
    // Output-neutral mutations weighted up so hit-path coverage stays dense.
    const MUTATIONS: [u64; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 5, 6, 6];
    let (mut hits, mut misses) = (0u32, 0u32);
    for i in 0..iters {
        let mutation = MUTATIONS[rng.pick(12) as usize];
        match mutation {
            0 => st.source_rev += 1,
            1 => st.early_probe = !st.early_probe,
            2 => st.tokens_ext = if st.tokens_ext == "ts" { "tsx" } else { "ts" },
            3 => st.app_pkg_rev += 1,
            4 => st.exports_theme2 = !st.exports_theme2,
            5 => st.theme_content_rev += 1,
            6 => st.readme_rev += 1,
            7 => st.dev = !st.dev,
            8 => st.dt_pkg_present = !st.dt_pkg_present,
            _ => unreachable!(),
        }
        st.sync(&fx);
        let source = st.source();
        let opts = fx.options(st.dev);
        let cold = fx.compile_cold(&source, &opts);
        let cached = fx.compile_cached(&store, &source, &opts, false);
        let context = format!("iter {i}, mutation {mutation}, state {st:?}");
        match (cold, cached) {
            (Ok(Some(cold)), Ok((Some(cached), status))) => {
                match status {
                    CacheStatus::Hit => hits += 1,
                    CacheStatus::Miss => misses += 1,
                    _ => {}
                }
                if let Err(report) = compare_results(&cached, &cold) {
                    panic!("cache diverged from cold compile ({context}, {status:?}): {report:?}");
                }
            }
            (Err(cold_err), Ok(_)) => {
                panic!("cache returned success where cold compile errors ({context}): {cold_err}")
            }
            (Ok(Some(_)), Err(cached_err)) => {
                panic!("cache path errored where cold compile succeeds ({context}): {cached_err}")
            }
            (Err(cold_err), Err(cached_err)) => {
                assert_eq!(
                    format!("{cold_err:?}"),
                    format!("{cached_err:?}"),
                    "error mismatch ({context})"
                );
            }
            (Ok(None), _) | (_, Ok((None, _))) => {
                panic!("pre-gate skipped a stylex source ({context})")
            }
        }
        // Periodic verify pass: a Poisoned status here is a purity violation
        // or cache bug regardless of which mutation preceded it.
        if i % 7 == 0
            && let Ok((_, status)) = fx.compile_cached(&store, &source, &opts, true)
            && let CacheStatus::Poisoned(report) = status
        {
            panic!("verify found a poisoned entry ({context}): {report:?}");
        }
    }
    if iters >= 20 {
        assert!(hits > 0, "fuzzer never exercised a cache hit");
        assert!(misses > 0, "fuzzer never exercised a cache miss");
    }
    println!("cache fuzz: {iters} iterations, {hits} hits, {misses} misses, seed {seed:#x}");
}
