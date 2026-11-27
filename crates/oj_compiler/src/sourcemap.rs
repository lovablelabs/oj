/// `data:application/json;base64,...` for a raw map JSON string (Vite's
/// genSourceMapUrl, sourcemap.ts).
pub fn map_json_to_data_url(json: &str) -> String {
    use base64::Engine as _;
    format!(
        "data:application/json;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(json)
    )
}

/// A map for edits that keep every line where it was (Start's compile):
/// each generated line maps to the same source line, a segment at every
/// token, so a token keeps its exact column wherever its line's text before
/// or after the edit is unchanged, and one inside the edit maps to where the
/// edit began. Columns count UTF-16 code units. A line count that differs
/// maps line for line from column 0.
pub fn line_map_json(source: &str, original: &str, generated: &str) -> String {
    let olines: Vec<&str> = original.split('\n').collect();
    let glines: Vec<&str> = generated.split('\n').collect();
    let same_count = olines.len() == glines.len();
    let mut mappings = String::new();
    let (mut prev_src_line, mut prev_src_col) = (0i64, 0i64);
    for (line, g) in glines.iter().enumerate() {
        if line > 0 {
            mappings.push(';');
        }
        let o = olines.get(line).copied().unwrap_or("");
        let gc: Vec<char> = g.chars().collect();
        let oc: Vec<char> = o.chars().collect();
        let units = |cs: &[char]| -> Vec<i64> {
            let mut out = Vec::with_capacity(cs.len() + 1);
            let mut n = 0i64;
            for c in cs {
                out.push(n);
                n += c.len_utf16() as i64;
            }
            out.push(n);
            out
        };
        let (gu, ou) = (units(&gc), units(&oc));
        let starts: Vec<usize> = if same_count {
            (0..gc.len())
                .filter(|&k| {
                    let c = gc[k];
                    !c.is_whitespace() && (k == 0 || !is_word(c) || !is_word(gc[k - 1]))
                })
                .collect()
        } else {
            Vec::new()
        };
        let prefix = gc.iter().zip(&oc).take_while(|(a, b)| a == b).count();
        let max_suffix = gc.len().min(oc.len()) - prefix;
        let suffix = gc
            .iter()
            .rev()
            .zip(oc.iter().rev())
            .take(max_suffix)
            .take_while(|(a, b)| a == b)
            .count();
        let mut segments: Vec<(i64, i64)> = Vec::new();
        if starts.is_empty() || starts[0] != 0 {
            segments.push((0, if prefix > 0 || !same_count { 0 } else { ou[0] }));
        }
        for k in starts {
            let src = if k < prefix {
                k
            } else if k >= gc.len() - suffix {
                oc.len() - (gc.len() - k)
            } else {
                prefix
            };
            segments.push((gu[k], ou[src.min(oc.len())]));
        }
        let mut prev_gen_col = 0i64;
        for (i, (gen_col, src_col)) in segments.into_iter().enumerate() {
            if i > 0 {
                mappings.push(',');
            }
            vlq(&mut mappings, gen_col - prev_gen_col);
            vlq(&mut mappings, 0);
            vlq(&mut mappings, line as i64 - prev_src_line);
            vlq(&mut mappings, src_col - prev_src_col);
            prev_gen_col = gen_col;
            prev_src_line = line as i64;
            prev_src_col = src_col;
        }
    }
    serde_json::json!({
        "version": 3,
        "sources": [source],
        "sourcesContent": [original],
        "names": [],
        "mappings": mappings,
    })
    .to_string()
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn vlq(out: &mut String, value: i64) {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut v = if value < 0 {
        ((-value) << 1) | 1
    } else {
        value << 1
    };
    loop {
        let mut digit = v & 31;
        v >>= 5;
        if v > 0 {
            digit |= 32;
        }
        out.push(B64[digit as usize] as char);
        if v == 0 {
            break;
        }
    }
}

/// A map an earlier transform of the module made, which oj's own map is
/// folded through.
pub enum InputMap<'a> {
    /// A plugin's map as it returned it; skipped when it does not parse.
    Json(&'a str),
    Parsed(oxc_sourcemap::SourceMap<'a>),
}

/// Folds oj's map through the `input_maps` (applied last to first) so served
/// positions trace to the original source, serializing the result once.
pub(crate) fn compose_input_maps(
    oj_map: &oxc_sourcemap::SourceMap,
    input_maps: &[InputMap<'_>],
) -> String {
    let decoded: Vec<Option<oxc_sourcemap::SourceMap>> = input_maps
        .iter()
        .map(|input| match input {
            InputMap::Json(json) => oxc_sourcemap::SourceMap::from_json_string(json).ok(),
            InputMap::Parsed(_) => None,
        })
        .collect();
    let mut folded: Option<oxc_sourcemap::SourceMap> = None;
    for (input, decoded) in input_maps.iter().zip(&decoded).rev() {
        let inner = match input {
            InputMap::Parsed(map) => map,
            InputMap::Json(_) => match decoded {
                Some(map) => map,
                None => continue,
            },
        };
        folded = Some(compose_two(folded.as_ref().unwrap_or(oj_map), inner));
    }
    folded.as_ref().unwrap_or(oj_map).to_json_string()
}

/// `outer` (served -> intermediate) through `inner` (intermediate -> original).
fn compose_two<'i>(
    outer: &oxc_sourcemap::SourceMap,
    inner: &'i oxc_sourcemap::SourceMap,
) -> oxc_sourcemap::SourceMap<'i> {
    let lut = inner.generate_lookup_table();
    let mut b = oxc_sourcemap::SourceMapBuilder::default();
    for t in outer.get_tokens() {
        if t.get_source_id().is_none() {
            continue;
        }
        if let Some(vt) =
            inner.lookup_source_view_token_approx(&lut, t.get_src_line(), t.get_src_col())
        {
            let src_id = vt
                .get_source()
                .map(|s| b.add_source_and_content(s, vt.get_source_content().unwrap_or("")));
            let name_id = vt.get_name().map(|n| b.add_name(n));
            b.add_token(
                t.get_dst_line(),
                t.get_dst_col(),
                vt.get_src_line(),
                vt.get_src_col(),
                src_id,
                name_id,
            );
        }
    }
    b.into_sourcemap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_map_keeps_columns_around_an_edit() {
        let original = "import a from \"a\";\nconst x = handler(), y = \"é\" + z;\nexport { x };\n";
        let generated =
            "import a from \"a\";\nconst x = rpc(\"id\"), y = \"é\" + z;\nexport { x };\n";
        let json = line_map_json("src/x.ts", original, generated);
        let map = oxc_sourcemap::SourceMap::from_json_string(&json).unwrap();
        let lookup = map.generate_lookup_table();
        let at = |line: u32, col: u32| {
            let t = map.lookup_token(&lookup, line, col).expect("a token");
            (t.get_src_line(), t.get_src_col())
        };
        // An unedited line keeps every column.
        assert_eq!(at(0, 7), (0, 7), "`a` on line 0");
        assert_eq!(at(2, 9), (2, 9), "`x` on line 2");
        // Before the edit on its line: exact.
        assert_eq!(at(1, 6), (1, 6), "`x` before the edit");
        // Inside the edit: where it began.
        assert_eq!(at(1, 10), (1, 10), "the edit's start");
        // After the edit on its line: shifted by the edit (`handler()` is
        // 9 units, `rpc("id")` 9 too, so the same column).
        let y_gen = generated.lines().nth(1).unwrap().find("y =").unwrap() as u32;
        let y_src = original.lines().nth(1).unwrap().find("y =").unwrap() as u32;
        assert_eq!(at(1, y_gen), (1, y_src), "`y` after the edit");
        // `z` after a non-ASCII string: UTF-16 columns.
        let line = generated.lines().nth(1).unwrap();
        let z = line.encode_utf16().count() as u32 - 1;
        assert_eq!(
            at(1, z),
            (
                1,
                original.lines().nth(1).unwrap().encode_utf16().count() as u32 - 1
            )
        );
        assert_eq!(map.get_source(0), Some("src/x.ts"));
    }

    #[test]
    fn a_shorter_edit_shifts_the_columns_after_it() {
        let original = "const x = createServerFnHandler(), tail = 1;\n";
        let generated = "const x = rpc(), tail = 1;\n";
        let json = line_map_json("a.ts", original, generated);
        let map = oxc_sourcemap::SourceMap::from_json_string(&json).unwrap();
        let lookup = map.generate_lookup_table();
        let t = map
            .lookup_token(&lookup, 0, generated.find("tail").unwrap() as u32)
            .unwrap();
        assert_eq!(t.get_src_col(), original.find("tail").unwrap() as u32);
    }

    #[test]
    fn compose_two_traces_served_position_back_to_original_source() {
        use oxc_sourcemap::SourceMapBuilder;
        // inner (plugin map): intermediate (0,0) -> original src.tsx (5,2).
        let mut ib = SourceMapBuilder::default();
        let sid = ib.add_source_and_content("src.tsx", "original source");
        ib.add_token(0, 0, 5, 2, Some(sid), None);
        let inner = ib.into_sourcemap();
        // outer (oj/oxc map): served (1,4) -> intermediate (0,0).
        let mut ob = SourceMapBuilder::default();
        let iid = ob.add_source_and_content("intermediate.js", "intermediate");
        ob.add_token(1, 4, 0, 0, Some(iid), None);
        let outer = ob.into_sourcemap();

        let composed = compose_two(&outer, &inner);
        let lut = composed.generate_lookup_table();
        let vt = composed
            .lookup_source_view_token(&lut, 1, 4)
            .expect("served (1,4) should map");
        // The served position now points at the ORIGINAL source, not the intermediate.
        assert_eq!(
            vt.get_source(),
            Some("src.tsx"),
            "source is the original file"
        );
        assert_eq!(
            vt.get_src_line(),
            5,
            "original line preserved through compose"
        );
        assert_eq!(
            vt.get_src_col(),
            2,
            "original column preserved through compose"
        );
    }

    #[test]
    fn compose_input_maps_folds_and_degrades_gracefully() {
        use oxc_sourcemap::{SourceMap, SourceMapBuilder};
        let mut ib = SourceMapBuilder::default();
        let sid = ib.add_source_and_content("app.tsx", "let x = 1;");
        ib.add_token(0, 0, 9, 3, Some(sid), None);
        let plugin_map = ib.into_sourcemap().to_json_string();

        let mut ob = SourceMapBuilder::default();
        let iid = ob.add_source_and_content("app.plugin.js", "intermediate");
        ob.add_token(0, 0, 0, 0, Some(iid), None);
        let oj_map = ob.into_sourcemap();

        // The fold traces through the plugin map to the original file.
        let folded = compose_two(&oj_map, &SourceMap::from_json_string(&plugin_map).unwrap())
            .to_json_string();
        assert!(
            folded.contains("app.tsx"),
            "folded map references the original source: {folded}"
        );

        // The composed map is raw JSON; the serve-time encoder turns it into
        // the inline data URL (Vite's genSourceMapUrl split).
        let json = compose_input_maps(&oj_map, &[InputMap::Json(&plugin_map)]);
        assert!(json.trim_start().starts_with('{'), "raw JSON map: {json}");
        let url = map_json_to_data_url(&json);
        assert!(
            url.starts_with("data:application/json") && url.contains("base64,"),
            "emits an inline data URL: {url}",
        );

        // Garbage input maps degrade to oj's own map rather than erroring.
        let fallback = compose_input_maps(&oj_map, &[InputMap::Json("not json")]);
        assert_eq!(fallback, oj_map.to_json_string());
    }

    /// The fold as it ran before maps were composed in memory: every step
    /// through a JSON string.
    fn compose_through_json(oj_map: &oxc_sourcemap::SourceMap, input_maps: &[String]) -> String {
        use oxc_sourcemap::SourceMap;
        let mut acc = oj_map.to_json_string();
        for pm in input_maps.iter().rev() {
            let outer = SourceMap::from_json_string(&acc).unwrap();
            let Ok(inner) = SourceMap::from_json_string(pm) else {
                continue;
            };
            acc = compose_two(&outer, &inner).to_json_string();
        }
        SourceMap::from_json_string(&acc).unwrap().to_json_string()
    }

    #[test]
    fn the_in_memory_fold_serializes_what_the_json_fold_did() {
        use oxc_sourcemap::SourceMapBuilder;
        let mut first = SourceMapBuilder::default();
        let id = first.add_source_and_content("app.tsx", "const é = 1;\nexport { é };\n");
        for (line, col) in [(0, 0), (0, 6), (0, 10), (2, 0), (2, 9)] {
            first.add_token(line, col, line.min(1), col / 2, Some(id), None);
        }
        let first = first.into_sourcemap();
        let mut second = SourceMapBuilder::default();
        let id = second.add_source_and_content("app.stylex.js", "intermediate one");
        let name = second.add_name("é");
        for (line, col) in [(0, 0), (1, 2), (1, 8), (3, 0), (3, 4)] {
            second.add_token(line, col, line.saturating_sub(1), col, Some(id), Some(name));
        }
        let second = second.into_sourcemap();
        let mut oj = SourceMapBuilder::default();
        let id = oj.add_source_and_content("app.plugin.js", "intermediate two");
        for (line, col) in [(0, 0), (0, 4), (1, 0), (2, 3), (4, 1), (5, 0)] {
            oj.add_token(line, col, line.min(3), col, Some(id), None);
        }
        oj.add_token(6, 0, 0, 0, None, None);
        let oj_map = oj.into_sourcemap();

        let json = [first.to_json_string(), second.to_json_string()];
        let garbage = "not json".to_string();
        for maps in [
            vec![],
            vec![json[0].clone()],
            vec![json[0].clone(), json[1].clone()],
            vec![json[0].clone(), garbage.clone(), json[1].clone()],
            vec![garbage.clone()],
        ] {
            let expected = compose_through_json(&oj_map, &maps);
            let as_json: Vec<InputMap> = maps.iter().map(|m| InputMap::Json(m)).collect();
            assert_eq!(compose_input_maps(&oj_map, &as_json), expected);
            let as_parsed: Vec<InputMap> = maps
                .iter()
                .map(|m| match oxc_sourcemap::SourceMap::from_json_string(m) {
                    Ok(map) => InputMap::Parsed(map),
                    Err(_) => InputMap::Json(m),
                })
                .collect();
            assert_eq!(compose_input_maps(&oj_map, &as_parsed), expected);
        }
    }
}
