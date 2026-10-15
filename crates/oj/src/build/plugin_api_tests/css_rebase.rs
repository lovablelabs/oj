// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;

use regex::Regex;

use super::{build_app, scratch, write_files};

/// A relative `url()` in an imported stylesheet is rebased to the emitted,
/// content-hashed asset under the default `/assets/` (it would 404 once the
/// css is concatenated into `assets/*.css`), the original relative spelling
/// is gone, the emitted file carries the source bytes, and a `data:` url is
/// left untouched (Vite's `skipUrlReplacer`). The asset is above the 4 KiB
/// `assetsInlineLimit`, so it is emitted rather than inlined, as in Vite.
/// (url() placement under a configured or empty `assetsDir` is pinned by
/// `assets_dir_places_every_hashed_output_under_it` and
/// `empty_assets_dir_with_relative_base_references_siblings`.)
#[tokio::test]
async fn relative_css_urls_rebase_to_hashed_assets_and_data_urls_stay() {
    let root = scratch("css-rebase");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"cssrebase","private":true}"#),
            (
                "index.html",
                r#"<!doctype html><html><head></head><body><script type="module" src="/src/main.tsx"></script></body></html>"#,
            ),
            (
                "src/main.tsx",
                "import \"./style.css\";\ndocument.body.dataset.ok = \"1\";\n",
            ),
            (
                "src/style.css",
                ".hero{background:url(./img/bg.png)}.d{background:url(data:image/gif;base64,AA)}",
            ),
        ],
    );
    let mut png = b"PNGDATA-oj-cssrebase".to_vec();
    png.extend(vec![1u8; 5000]);
    fs::create_dir_all(root.join("src/img")).unwrap();
    fs::write(root.join("src/img/bg.png"), &png).unwrap();

    build_app(&root).await.expect("the css-rebase app builds");

    let assets = root.join("dist/assets");
    let files: Vec<String> = fs::read_dir(&assets)
        .expect("dist/assets exists")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let style = files
        .iter()
        .find(|f| f.ends_with(".css"))
        .unwrap_or_else(|| panic!("a .css is emitted: {files:?}"));
    let css = fs::read_to_string(assets.join(style)).unwrap();

    assert!(
        Regex::new(r#"url\("/assets/bg-[0-9a-f]+\.png"\)"#)
            .unwrap()
            .is_match(&css),
        "relative url() rebased to a hashed /assets/ file:\n{css}"
    );
    assert!(
        !css.contains("./img/bg.png"),
        "the original relative url() is gone:\n{css}"
    );
    assert!(
        css.contains("url(data:image/gif;base64,AA)"),
        "a data: url() is left untouched:\n{css}"
    );

    let png_re = Regex::new(r"^bg-[0-9a-f]+\.png$").unwrap();
    let emitted = files
        .iter()
        .find(|f| png_re.is_match(f))
        .unwrap_or_else(|| panic!("the referenced png is emitted into assets/: {files:?}"));
    let bytes = fs::read(assets.join(emitted)).unwrap();
    assert_eq!(bytes, png, "the emitted png carries the source bytes");
}
