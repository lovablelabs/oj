use oj_stylex::hash::{create_short_hash, hash};

#[derive(serde::Deserialize)]
struct HashPin {
    input: String,
    murmur_base36: String,
    short_hash: String,
}

#[test]
fn classic_known_answers() {
    // color: 'blue' -> .xju2f9n ; backgroundColor: 'red' -> .xrkmrrc (prefix 'x' + hash).
    assert_eq!(hash("<>colorbluenull"), "ju2f9n");
    assert_eq!(hash("<>background-colorrednull"), "rkmrrc");
    // Minified keys: color -> kMwMTN ; backgroundColor -> kWkggS (prefix 'k' + short hash).
    assert_eq!(create_short_hash("<>color"), "MwMTN");
    assert_eq!(create_short_hash("<>backgroundColor"), "WkggS");
}

#[test]
fn oracle_generated_pins() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/pins/hash-pins.json"
    ))
    .expect("run crates/stylex/conformance/src/gen-pins.mjs to generate pins");
    let pins: Vec<HashPin> = serde_json::from_str(&raw).unwrap();
    assert!(pins.len() >= 20, "suspiciously small pin set");
    for pin in pins {
        assert_eq!(hash(&pin.input), pin.murmur_base36, "hash({:?})", pin.input);
        assert_eq!(
            create_short_hash(&pin.input),
            pin.short_hash,
            "short_hash({:?})",
            pin.input
        );
    }
}
