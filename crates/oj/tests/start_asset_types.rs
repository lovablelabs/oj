// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A file of a type Vite knows as an asset (`KNOWN_ASSET_TYPES`: images,
//! media, fonts, pdf, txt) imports as its URL in a Start client bundle, in
//! dev and in the build, and serves with its content type. A type outside
//! oj's former, shorter list (an mp3) reached rolldown as a JavaScript
//! module, which failed the client bundle on its first byte that is not
//! UTF-8 ("stream did not contain valid UTF-8").

#[allow(dead_code)]
mod start_app;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;

use start_app::{app, build, Server};

/// An ID3 header and a frame sync: bytes no UTF-8 decoder accepts.
const MP3: &[u8] = b"ID3\x03\x00\x00\x00\x00\x00\x0a\xff\xfb\x90\x00\x80\x81\xfe\xfd";

/// The fixture app with an `<audio>` on its home route, its source an
/// imported mp3.
fn with_sound(root: &Path) {
    std::fs::write(root.join("src/widgets/sound.mp3"), MP3).unwrap();
    std::fs::write(
        root.join("src/widgets/sound.jsx"),
        "import song from \"./sound.mp3\";\nexport function Sound() {\n  return <audio data-testid=\"sound\" src={song} />;\n}\n",
    )
    .unwrap();
    let index = root.join("src/routes/index.tsx");
    let source = std::fs::read_to_string(&index).unwrap();
    let source = source.replacen(
        "import { GlobWidget } from \"../widgets/glob-widget.jsx\";",
        "import { GlobWidget } from \"../widgets/glob-widget.jsx\";\nimport { Sound } from \"../widgets/sound.jsx\";",
        1,
    );
    let patched = source.replacen(
        "<p data-testid=\"glob-jsx\"><GlobWidget /></p>",
        "<p data-testid=\"glob-jsx\"><GlobWidget /></p>\n      <Sound />",
        1,
    );
    assert_ne!(patched, source, "the fixture's glob widget moved");
    std::fs::write(&index, patched).unwrap();
}

/// The `src` of the home document's audio element.
fn audio_src(home: &str) -> String {
    let tag = home
        .split("<audio")
        .nth(1)
        .unwrap_or_else(|| panic!("no audio element:\n{home}"));
    let src = tag
        .split("src=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .unwrap_or_else(|| panic!("the audio element has no src:\n{tag}"));
    src.replace("&amp;", "&")
}

/// One raw GET: the status, the headers and the body's bytes (the shared
/// helper decodes the body as text, which these bytes are not).
fn get_bytes(port: u16, path: &str) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    let split = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a header end");
    let head = String::from_utf8_lossy(&bytes[..split]).into_owned();
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (
        status,
        head.to_ascii_lowercase(),
        bytes[split + 4..].to_vec(),
    )
}

#[test]
fn an_mp3_import_is_its_url_in_dev() {
    let Some(app) = app("asset-mp3-dev") else {
        return;
    };
    let root = app.0.clone();
    with_sound(&root);
    let dev = Server::oj_dev(&root);
    let (status, home) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "{home}");
    let src = audio_src(&home);
    assert!(src.ends_with("/sound.mp3"), "{src}");
    let (status, head, body) = get_bytes(dev.port, &src);
    assert_eq!(status, 200, "{head}");
    assert!(head.contains("content-type: audio/mpeg"), "{head}");
    assert_eq!(body, MP3, "the file's bytes, as they are");
    // The client bundle took the import as an asset: its chunk names the
    // URL, and nothing read the bytes as source.
    let (status, entry) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    assert_eq!(status, 200);
    assert!(
        entry.contains(&src),
        "the entry names the asset's url:\n{entry}"
    );
    let output = dev.output();
    assert!(
        !output.contains("did not contain valid UTF-8") && !output.contains("client bundle failed"),
        "{output}"
    );
}

#[test]
fn an_mp3_import_is_an_emitted_asset_in_the_build() {
    let Some(app) = app("asset-mp3-build") else {
        return;
    };
    let root = app.0.clone();
    with_sound(&root);
    build(&root);
    let mut emitted = Vec::new();
    let mut stack = vec![root.join("dist")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "mp3") {
                emitted.push(path);
            }
        }
    }
    assert!(!emitted.is_empty(), "no mp3 emitted under dist");
    for file in &emitted {
        assert_eq!(std::fs::read(file).unwrap(), MP3, "{}", file.display());
    }
}
