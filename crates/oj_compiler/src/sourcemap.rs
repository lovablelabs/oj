/// `data:application/json;base64,...` for a raw map JSON string (Vite's
/// genSourceMapUrl, sourcemap.ts).
pub fn map_json_to_data_url(json: &str) -> String {
    use base64::Engine as _;
    format!(
        "data:application/json;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(json)
    )
}

pub(crate) fn compose_input_maps_json(
    oj_map: &oxc_sourcemap::SourceMap,
    input_maps: &[String],
) -> String {
    let mut acc = oj_map.to_json_string();
    for pm in input_maps.iter().rev() {
        let outer = match oxc_sourcemap::SourceMap::from_json_string(&acc) {
            Ok(m) => m,
            Err(_) => return oj_map.to_data_url(),
        };
        let inner = match oxc_sourcemap::SourceMap::from_json_string(pm) {
            Ok(m) => m,
            Err(_) => continue,
        };
        acc = compose_two(&outer, &inner).to_json_string();
    }
    match oxc_sourcemap::SourceMap::from_json_string(&acc) {
        Ok(m) => m.to_json_string(),
        Err(_) => oj_map.to_json_string(),
    }
}

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
    fn compose_input_maps_json_folds_and_degrades_gracefully() {
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
        let json = compose_input_maps_json(&oj_map, &[plugin_map]);
        assert!(json.trim_start().starts_with('{'), "raw JSON map: {json}");
        let url = map_json_to_data_url(&json);
        assert!(
            url.starts_with("data:application/json") && url.contains("base64,"),
            "emits an inline data URL: {url}",
        );

        // Garbage input maps degrade to oj's own map rather than erroring.
        let fallback = compose_input_maps_json(&oj_map, &["not json".to_string()]);
        assert!(fallback.trim_start().starts_with('{'));
    }
}
