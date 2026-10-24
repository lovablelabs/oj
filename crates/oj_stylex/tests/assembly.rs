// Replays testdata/pins/assembly/*.json byte-exact against the real
// processStylexRules outputs (see conformance/src/gen-pins-assembly.mjs).

use std::collections::HashMap;
use std::path::PathBuf;

use oj_stylex::assemble::{AssembleConfig, LayersConfig, RuleRegistry, assemble};
use oj_stylex::rules::StylexRule;

fn pins_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/pins/assembly")
}

struct PinsFile {
    name: String,
    pool: Vec<StylexRule>,
    cases: Vec<serde_json::Value>,
}

fn load_pins() -> Vec<PinsFile> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(pins_dir())
        .expect("testdata/pins/assembly missing — run conformance/src/gen-pins-assembly.mjs")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no pin files found");
    files
        .into_iter()
        .map(|path| {
            let doc: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let pool = doc["pool"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| StylexRule::from_metadata_tuple(t).unwrap())
                .collect();
            PinsFile {
                name: path.file_stem().unwrap().to_string_lossy().into_owned(),
                pool,
                cases: doc["cases"].as_array().unwrap().clone(),
            }
        })
        .collect()
}

fn case_rules(pool: &[StylexRule], case: &serde_json::Value) -> Vec<StylexRule> {
    case["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| pool[i.as_u64().unwrap() as usize].clone())
        .collect()
}

#[test]
fn golden_replay_byte_exact() {
    let mut total = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for pins in load_pins() {
        for case in &pins.cases {
            total += 1;
            let name = format!("{}::{}", pins.name, case["name"].as_str().unwrap());
            let rules = case_rules(&pins.pool, case);
            let cfg = match AssembleConfig::from_json(&case["cfg"]) {
                Ok(cfg) => cfg,
                Err(e) => {
                    failures.push(format!("{name}: cfg parse failed: {e}"));
                    continue;
                }
            };
            let got = assemble(&rules, &cfg);
            let mut registry = RuleRegistry::new();
            registry.set_file_rules("input.ts", rules);
            assert_eq!(registry.emit(&cfg), got, "registry: {name}");
            match (case.get("css").and_then(|c| c.as_str()), got) {
                (Some(want), Ok(css)) => {
                    if css != want {
                        let at = css
                            .bytes()
                            .zip(want.bytes())
                            .position(|(a, b)| a != b)
                            .unwrap_or_else(|| css.len().min(want.len()));
                        let lo = at.saturating_sub(60);
                        failures.push(format!(
                            "{name}: css diverges at byte {at}\n  want …{:?}\n  got  …{:?}",
                            &want[lo..(at + 60).min(want.len())],
                            &css[lo..(at + 60).min(css.len())],
                        ));
                    }
                }
                (Some(_), Err(e)) => failures.push(format!("{name}: unexpected error {e}")),
                (None, Ok(_)) => {
                    failures.push(format!(
                        "{name}: expected error {:?}, got css",
                        case["error"].as_str()
                    ));
                }
                (None, Err(e)) => {
                    let want = case["error"].as_str().unwrap();
                    if e.to_string() != want {
                        failures.push(format!(
                            "{name}: error text mismatch\n  want {want:?}\n  got  {:?}",
                            e.to_string()
                        ));
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{}/{} golden cases failed:\n{}",
        failures.len(),
        total,
        failures.join("\n")
    );
    assert!(total > 500, "suspiciously few golden cases: {total}");
}

#[test]
fn config_from_json_forms() {
    let cfg = AssembleConfig::from_json(&serde_json::json!(true)).unwrap();
    assert!(matches!(cfg.use_layers, LayersConfig::On { .. }));
    let cfg = AssembleConfig::from_json(&serde_json::json!(false)).unwrap();
    assert_eq!(cfg.use_layers, LayersConfig::Off);
    let cfg = AssembleConfig::from_json(&serde_json::json!({
        "useLayers": {"before": ["a"], "prefix": "p"},
        "useLegacyClassnamesSort": true,
    }))
    .unwrap();
    assert_eq!(
        cfg.use_layers,
        LayersConfig::On {
            before: vec!["a".into()],
            after: vec![],
            prefix: Some("p".into())
        }
    );
    assert!(cfg.use_legacy_classnames_sort);
    let cfg =
        AssembleConfig::from_json(&serde_json::json!({"enableLTRRTLComments": true})).unwrap();
    assert!(cfg.enable_ltr_rtl_comments);
    // Falsy value keeps the option off, mirroring the upstream destructuring default.
    let cfg =
        AssembleConfig::from_json(&serde_json::json!({"enableLTRRTLComments": false})).unwrap();
    assert!(!cfg.enable_ltr_rtl_comments);
}

// r2 finding 11: const-target trimming is ES TrimString — FEFF must be trimmed
// (Rust `str::trim` leaves it, which rewrites the wrong declaration key).
#[test]
fn const_var_rewrite_trims_es_whitespace() {
    let rules = vec![
        StylexRule {
            class_name: "xalias".into(),
            ltr: "".into(),
            rtl: None,
            const_key: Some("xalias".into()),
            const_val: Some(Box::new(serde_json::json!(
                "var( \u{FEFF}--real-token , 16px)"
            ))),
            priority: 0.0,
        },
        StylexRule {
            class_name: "xroot".into(),
            ltr: ":root{--xalias:var(--xalias)}".into(),
            rtl: None,
            const_key: None,
            const_val: None,
            priority: 100.0,
        },
    ];
    let css = assemble(&rules, &AssembleConfig::default()).unwrap();
    assert_eq!(css, ":root{--real-token:var( \u{FEFF}--real-token , 16px)}");
}

#[test]
fn ltr_rtl_comments_wrap_instead_of_ancestor_selectors() {
    let rules = vec![StylexRule {
        class_name: "xa".into(),
        ltr: ".xa{margin-left:8px}".into(),
        rtl: Some(".xa{margin-right:8px}".into()),
        const_key: None,
        const_val: None,
        priority: 4000.0,
    }];
    let cfg =
        AssembleConfig::from_json(&serde_json::json!({"enableLTRRTLComments": true})).unwrap();
    let css = assemble(&rules, &cfg).unwrap();
    assert_eq!(
        css,
        "/* @ltr begin */.xa{margin-left:8px}/* @ltr end */\n/* @rtl begin */.xa{margin-right:8px}/* @rtl end */"
    );
}

// r2 finding 5: tagged non-finite constVal decodes through assembly.
#[test]
fn non_finite_const_substitutes_js_number_strings() {
    let mut rules = Vec::new();
    for (name, tag) in [("xinf", "Infinity"), ("xnan", "NaN")] {
        rules.push(StylexRule {
            class_name: name.into(),
            ltr: "".into(),
            rtl: None,
            const_key: Some(name.into()),
            const_val: Some(Box::new(serde_json::json!({ "__jsnum": tag }))),
            priority: 0.0,
        });
    }
    rules.push(StylexRule {
        class_name: "zuse".into(),
        ltr: ".zuse{z-index:var(--xinf)}".into(),
        rtl: None,
        const_key: None,
        const_val: None,
        priority: 3000.0,
    });
    let css = assemble(&rules, &AssembleConfig::default()).unwrap();
    assert_eq!(css, ".zuse{z-index:Infinity}");
}

// Deterministic LCG so the sequence is reproducible in failures.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[test]
fn registry_matches_one_shot_over_random_edit_sequences() {
    let pins = load_pins();
    let universe: Vec<StylexRule> = pins.iter().flat_map(|p| p.pool.iter().cloned()).collect();
    assert!(universe.len() > 100);

    let configs = [
        AssembleConfig::from_json(&serde_json::json!({"useLayers": true})).unwrap(),
        AssembleConfig::from_json(&serde_json::json!({"useLayers": false})).unwrap(),
        AssembleConfig::from_json(
            &serde_json::json!({"useLayers": false, "useLegacyClassnamesSort": true}),
        )
        .unwrap(),
    ];
    let file_keys: Vec<String> = (0..8).map(|i| format!("src/file{i}.ts")).collect();

    let mut rng = Lcg(0x5eed_1234_abcd_0001);
    let mut registry = RuleRegistry::new();
    let mut shadow: HashMap<String, Vec<StylexRule>> = HashMap::new();

    for step in 0..300 {
        let key = &file_keys[rng.below(file_keys.len())];
        if rng.below(4) == 0 {
            registry.remove_file(key);
            shadow.remove(key);
        } else {
            let n = rng.below(24);
            let rules: Vec<StylexRule> = (0..n)
                .map(|_| universe[rng.below(universe.len())].clone())
                .collect();
            registry.set_file_rules(key, rules.clone());
            shadow.insert(key.clone(), rules);
        }

        let cfg = &configs[step % configs.len()];
        let mut keys: Vec<&String> = shadow.keys().collect();
        keys.sort();
        let one_shot_input: Vec<StylexRule> = keys
            .iter()
            .flat_map(|k| shadow[*k].iter().cloned())
            .collect();
        let expected = assemble(&one_shot_input, cfg);
        let emitted = registry.emit(cfg);
        assert_eq!(
            emitted, expected,
            "step {step} diverged from one-shot assemble"
        );
        // Memoized re-emit must be identical and must not bump the generation.
        let generation = registry.generation();
        assert_eq!(registry.emit(cfg), expected, "step {step} re-emit diverged");
        assert_eq!(registry.generation(), generation);
    }
}

#[test]
fn registry_noop_set_keeps_generation() {
    let mut registry = RuleRegistry::new();
    let rule = StylexRule {
        class_name: "xa".into(),
        ltr: ".xa{color:red}".into(),
        rtl: None,
        const_key: None,
        const_val: None,
        priority: 3000.0,
    };
    registry.set_file_rules("a.ts", vec![rule.clone()]);
    let generation = registry.generation();
    registry.set_file_rules("a.ts", vec![rule]);
    assert_eq!(registry.generation(), generation);
    assert!(!registry.remove_file("missing.ts"));
    assert_eq!(registry.generation(), generation);
}

// The keyed fast sort and the generic comparator path must order identically;
// replaying every golden case pins the equivalence.
#[test]
fn keyed_sort_matches_comparator_path_on_all_goldens() {
    use oj_stylex::assemble::{assemble_with_comparator, default_locale_cmp};
    let mut total = 0usize;
    for pins in load_pins() {
        for case in &pins.cases {
            let Ok(cfg) = AssembleConfig::from_json(&case["cfg"]) else {
                continue;
            };
            let rules = case_rules(&pins.pool, case);
            assert_eq!(
                assemble(&rules, &cfg),
                assemble_with_comparator(&rules, &cfg, default_locale_cmp),
                "{}::{}",
                pins.name,
                case["name"].as_str().unwrap()
            );
            total += 1;
        }
    }
    assert!(total > 500, "suspiciously few cases: {total}");
}

// Swap-only edit sequences keep the incremental fast path engaged (no
// structural changes); the debug cross-check inside emit also verifies each step.
#[test]
fn registry_incremental_swap_parity() {
    let pins = load_pins();
    let universe: Vec<StylexRule> = pins.iter().flat_map(|p| p.pool.iter().cloned()).collect();
    let configs = [
        AssembleConfig::from_json(&serde_json::json!({"useLayers": true})).unwrap(),
        AssembleConfig::from_json(&serde_json::json!(false)).unwrap(),
    ];
    let mut rng = Lcg(0xfeed_beef_0000_0001);
    let mut registry = RuleRegistry::new();
    let file_keys: Vec<String> = (0..6).map(|i| format!("src/f{i}.ts")).collect();
    for key in &file_keys {
        let rules: Vec<StylexRule> = (0..12)
            .map(|_| universe[rng.below(universe.len())].clone())
            .collect();
        registry.set_file_rules(key, rules);
    }
    for step in 0..120 {
        let key = &file_keys[rng.below(file_keys.len())];
        let n = 1 + rng.below(16);
        let rules: Vec<StylexRule> = (0..n)
            .map(|_| universe[rng.below(universe.len())].clone())
            .collect();
        registry.set_file_rules(key, rules);
        let cfg = &configs[step % configs.len()];
        assert_eq!(
            registry.emit(cfg),
            assemble(&registry.all_rules(), cfg),
            "step {step} diverged"
        );
    }
}

fn plain_rule(class_name: &str, ltr: &str, priority: f64) -> StylexRule {
    StylexRule {
        class_name: class_name.into(),
        ltr: ltr.into(),
        rtl: None,
        const_key: None,
        const_val: None,
        priority,
    }
}

#[test]
fn registry_shared_owners_match_full_assembly_across_batches() {
    let mut registry = RuleRegistry::new();
    let rules: Vec<_> = (0..32)
        .map(|i| plain_rule(&format!("x{i}"), &format!(".x{i}{{width:{i}px}}"), 3000.0))
        .collect();
    let configs = [
        AssembleConfig::default(),
        AssembleConfig::from_json(&serde_json::json!({"useLayers": true})).unwrap(),
        AssembleConfig::from_json(&serde_json::json!({"useLegacyClassnamesSort": true})).unwrap(),
    ];
    for i in 0..64 {
        registry.set_file_rules(&format!("{i:03}.ts"), rules.clone());
    }
    let mut rng = Lcg(0xaabb_ccdd_1234_5678);
    for step in 0..100 {
        for _ in 0..1 + rng.below(32) {
            let path = format!("{:03}.ts", rng.below(64));
            let mut next: Vec<_> = (0..rng.below(32))
                .map(|_| rules[rng.below(rules.len())].clone())
                .collect();
            if step % 19 == 0 {
                next.push(plain_rule("x0", ".x0{width:999px}", 2000.0));
            }
            if step % 23 == 0 {
                registry.remove_file(&path);
            } else {
                registry.set_file_rules(&path, next);
            }
        }
        for cfg in &configs {
            assert_eq!(
                registry.emit(cfg),
                assemble(&registry.all_rules(), cfg),
                "step {step}"
            );
        }
    }
}

// Chars outside the pinned collation alphabet and non-finite priorities are
// outside the keyed-order preconditions: swaps must still match one-shot.
#[test]
fn registry_unsafe_rules_fall_back_to_full_path() {
    let cfg = AssembleConfig::default();
    for odd in [
        plain_rule("xa", ".xa{content:\"日\"}", 3000.0),
        plain_rule("xb", ".xb{color:red}", f64::INFINITY),
        plain_rule("xc", ".xc{color:red}", f64::NAN),
    ] {
        let mut registry = RuleRegistry::new();
        registry.set_file_rules("a.ts", vec![plain_rule("xd", ".xd{margin:0}", 2000.0)]);
        registry.set_file_rules("b.ts", vec![odd.clone()]);
        assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));
        registry.set_file_rules("a.ts", vec![plain_rule("xe", ".xe{margin:1px}", 2000.0)]);
        assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));
    }
}

#[test]
fn registry_const_value_swap_reassembles() {
    let cfg = AssembleConfig::default();
    let const_rule = |val: &str| StylexRule {
        class_name: "xtok".into(),
        ltr: "".into(),
        rtl: None,
        const_key: Some("xtok".into()),
        const_val: Some(Box::new(serde_json::json!(val))),
        priority: 0.0,
    };
    let user = plain_rule("xuse", ".xuse{gap:var(--xtok)}", 3000.0);
    let mut registry = RuleRegistry::new();
    registry.set_file_rules("consts.ts", vec![const_rule("8px")]);
    registry.set_file_rules("app.ts", vec![user.clone()]);
    let css = registry.emit(&cfg).unwrap();
    assert!(css.contains("gap:8px"), "{css}");
    registry.set_file_rules("consts.ts", vec![const_rule("12px")]);
    let css = registry.emit(&cfg).unwrap();
    assert!(css.contains("gap:12px"), "{css}");
    assert_eq!(Ok(css), assemble(&registry.all_rules(), &cfg));
}

// Exact duplicates are dropped before the keyed sort; the undeduped comparator
// path pins the pre-dedupe bytes on every golden case, in two duplication shapes.
#[test]
fn duplicate_rules_match_comparator_path_on_all_goldens() {
    use oj_stylex::assemble::{assemble_with_comparator, default_locale_cmp};
    let mut rng = Lcg(0xd0d0_cafe_0000_0001);
    let mut total = 0usize;
    for pins in load_pins() {
        for case in &pins.cases {
            let Ok(cfg) = AssembleConfig::from_json(&case["cfg"]) else {
                continue;
            };
            let base = case_rules(&pins.pool, case);
            if base.is_empty() {
                continue;
            }
            let mut appended = base.clone();
            appended.extend((0..base.len() * 3).map(|_| base[rng.below(base.len())].clone()));
            let shuffled: Vec<StylexRule> = (0..base.len() * 4)
                .map(|_| base[rng.below(base.len())].clone())
                .collect();
            for rules in [appended, shuffled] {
                assert_eq!(
                    assemble(&rules, &cfg),
                    assemble_with_comparator(&rules, &cfg, default_locale_cmp),
                    "{}::{}",
                    pins.name,
                    case["name"].as_str().unwrap()
                );
            }
            total += 1;
        }
    }
    assert!(total > 500, "suspiciously few cases: {total}");
}

// A class carrying two distinct texts keeps every copy: dropping the later copy
// would hand the last-wins Map a different winner.
#[test]
fn duplicates_of_a_multi_text_class_keep_the_last_wins_winner() {
    use oj_stylex::assemble::{assemble_with_comparator, default_locale_cmp};
    let legacy =
        AssembleConfig::from_json(&serde_json::json!({"useLegacyClassnamesSort": true})).unwrap();
    let red = plain_rule("xc", ".xc{color:red}", 3000.0);
    let blue = plain_rule("xc", ".xc{color:blue}", 3000.0);
    let rules = vec![red.clone(), blue, red];
    assert_eq!(assemble(&rules, &legacy), Ok(".xc{color:red}".to_string()));
    assert_eq!(
        assemble(&rules, &legacy),
        assemble_with_comparator(&rules, &legacy, default_locale_cmp)
    );

    let cfg = AssembleConfig::default();
    let plain = plain_rule("xm", ".xm{margin-left:8px}", 3000.0);
    let mut with_rtl = plain.clone();
    with_rtl.rtl = Some(".xm{margin-right:8px}".into());
    let rules = vec![plain.clone(), with_rtl, plain];
    assert_eq!(
        assemble(&rules, &cfg),
        Ok(".xm{margin-left:8px}".to_string())
    );
    assert_eq!(
        assemble(&rules, &cfg),
        assemble_with_comparator(&rules, &cfg, default_locale_cmp)
    );
}

// An unverified selector char leaves the declaration slice verified: the declaration step
// still collates (`{color` before `{Color`), only the whole-rule step falls back to UTF-16.
#[test]
fn unverified_selector_char_keeps_declaration_collation() {
    use oj_stylex::assemble::{assemble_with_comparator, default_locale_cmp};
    let configs = [
        AssembleConfig::default(),
        AssembleConfig::from_json(&serde_json::json!({"useLegacyClassnamesSort": true})).unwrap(),
    ];
    let mut rules = vec![
        plain_rule("xb", ".xb{Color:red}", 3000.0),
        plain_rule("xa", ".xa[title=\"\u{2603}\"]{color:red}", 3000.0),
        plain_rule("x\u{2603}", ".x\u{2603}{color:blue}", 3000.0),
        plain_rule("xc", ".xc[title=\"\u{2603}\"]{color:red}", 3000.0),
    ];
    assert_eq!(
        assemble(&rules, &configs[0]).unwrap(),
        ".x\u{2603}{color:blue}\n.xa[title=\"\u{2603}\"]{color:red}\n.xc[title=\"\u{2603}\"]{color:red}\n.xb{Color:red}"
    );
    rules.push(plain_rule("xd", ".xd{content:\"\u{2603}\"}", 3000.0));
    for cfg in &configs {
        assert_eq!(
            assemble(&rules, cfg),
            assemble_with_comparator(&rules, cfg, default_locale_cmp)
        );
    }
}

// Unverified collation chars and non-finite priorities void the dedupe
// preconditions: the full input must sort exactly as the comparator path does.
#[test]
fn duplicates_outside_keyed_preconditions_match_comparator_path() {
    use oj_stylex::assemble::{assemble_with_comparator, default_locale_cmp};
    let configs = [
        AssembleConfig::default(),
        AssembleConfig::from_json(&serde_json::json!({"useLegacyClassnamesSort": true})).unwrap(),
    ];
    for odd in [
        plain_rule("xa", ".xa{content:\"日\"}", 3000.0),
        plain_rule("xb", ".xb{color:red}", f64::INFINITY),
        plain_rule("xc", ".xc{color:red}", f64::NAN),
    ] {
        let dup = plain_rule("xd", ".xd{margin:0}", 3000.0);
        let rules = vec![dup.clone(), odd.clone(), dup.clone(), odd, dup];
        for cfg in &configs {
            assert_eq!(
                assemble(&rules, cfg),
                assemble_with_comparator(&rules, cfg, default_locale_cmp)
            );
        }
    }
}
