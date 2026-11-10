// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A server route answering with a `ReadableStream` body (server-sent
//! events, a chunked download) delivers each chunk as the handler writes
//! it, as Vite's dev middleware for Start does: the Start plugin hands the
//! handler's Response to srvx's `sendNodeResponse`, which pipes the body
//! into Node's response (`streamBody`: one `res.write` per chunk). oj's
//! in-process runner read the whole body before replying, so a stream
//! writing a chunk every second reached the browser in one burst once the
//! handler closed it. The route here writes a chunk, waits half a second,
//! writes another and closes; the first chunk must be in hand well before
//! the end of the body.
//!
//! The app is the Start fixture (see `start_app`) with the streaming route
//! added.

#[allow(dead_code)]
mod start_app;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

use start_app::{app, build, Server, WAIT};

/// How long the route waits between its two chunks.
const GAP: Duration = Duration::from_millis(500);

const ROUTE: &str = r#"import { createRoute } from "@tanstack/react-router";

import { rootRoute } from "./__root";

// Two server-sent events, the second half a second after the first, as a
// progress feed writes them.
export const streamRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/api/stream",
  server: {
    handlers: {
      GET: () => {
        const encoder = new TextEncoder();
        const body = new ReadableStream<Uint8Array>({
          start(controller) {
            controller.enqueue(encoder.encode("data: first-chunk\n\n"));
            setTimeout(() => {
              controller.enqueue(encoder.encode("data: second-chunk\n\n"));
              controller.close();
            }, 500);
          },
        });
        return new Response(body, {
          headers: { "content-type": "text/event-stream", "cache-control": "no-cache" },
        });
      },
    },
  },
});
"#;

fn write_app(root: &Path) {
    std::fs::write(root.join("src/routes/stream.tsx"), ROUTE).unwrap();
    let tree = root.join("src/routeTree.ts");
    let source = std::fs::read_to_string(&tree).unwrap();
    assert!(source.contains("resolveErrorRoute]"), "{source}");
    let source = source
        .replacen(
            "import { rootRoute }",
            "import { streamRoute } from \"./routes/stream\";\nimport { rootRoute }",
            1,
        )
        .replacen("resolveErrorRoute]", "resolveErrorRoute, streamRoute]", 1);
    std::fs::write(&tree, source).unwrap();
}

/// One request for the stream, read as it arrives: when the first chunk's
/// text was in hand, when the second's was, and the whole response.
fn read_stream(server: &Server) -> (Duration, Duration, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream
        .write_all(
            format!(
                "GET /api/stream HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                server.port
            )
            .as_bytes(),
        )
        .unwrap();
    let started = Instant::now();
    let mut bytes = Vec::new();
    let mut first = None;
    let mut second = None;
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&bytes);
        if first.is_none() && text.contains("first-chunk") {
            first = Some(started.elapsed());
        }
        if second.is_none() && text.contains("second-chunk") {
            second = Some(started.elapsed());
        }
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let first = first.unwrap_or_else(|| panic!("no first chunk in\n{text}\n{}", server.output()));
    let second =
        second.unwrap_or_else(|| panic!("no second chunk in\n{text}\n{}", server.output()));
    (first, second, text)
}

fn streams_live(label: &str, server: &Server) {
    // The page first, so the dev server's boot work is behind the request
    // that is timed.
    let (status, _) = server.request("GET", "/", &[]);
    assert_eq!(status, 200, "{label}: the page\n{}", server.output());
    let (first, second, text) = read_stream(server);
    assert!(
        text.contains("text/event-stream"),
        "{label}: the stream's content type\n{text}"
    );
    // The handler writes the second chunk GAP after the first; a first chunk
    // held until the body ended arrives with the second. Half the gap leaves
    // room for a loaded machine.
    assert!(
        second.saturating_sub(first) >= GAP / 2,
        "{label}: the first chunk arrived at {first:?} and the second at {second:?}; \
         the first was held until the body ended\n{text}\n{}",
        server.output()
    );
}

#[test]
fn a_streamed_server_route_body_arrives_chunk_by_chunk() {
    let Some(app) = app("streaming-bodies") else {
        return;
    };
    let root = &app.0;
    write_app(root);

    let dev = Server::oj_dev(root);
    streams_live("dev", &dev);
    drop(dev);

    build(root);
    let built = Server::node_server(root);
    streams_live("build", &built);
}
