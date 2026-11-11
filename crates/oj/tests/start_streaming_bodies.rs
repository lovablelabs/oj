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
//! added. The route's gap between its chunks is `?gap=<ms>` (500 by default),
//! so a feed that idles for long can be opened too.

#[allow(dead_code)]
mod start_app;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

use start_app::{app, build, Server, WAIT};

/// How long the route waits between its two chunks.
const GAP: Duration = Duration::from_millis(500);

/// How long the idle feed of the respawn test waits between its chunks.
const IDLE: Duration = Duration::from_secs(10);

const ROUTE: &str = r#"import { createRoute } from "@tanstack/react-router";

import { rootRoute } from "./__root";

// Two server-sent events, the second `?gap=` ms (half a second by default)
// after the first, as a progress feed writes them.
export const streamRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/api/stream",
  server: {
    handlers: {
      GET: ({ request }) => {
        const gap = Number(new URL(request.url).searchParams.get("gap") ?? "500");
        const encoder = new TextEncoder();
        const body = new ReadableStream<Uint8Array>({
          start(controller) {
            controller.enqueue(encoder.encode("data: first-chunk\n\n"));
            setTimeout(() => {
              controller.enqueue(encoder.encode("data: second-chunk\n\n"));
              controller.close();
            }, gap);
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

/// A request for `path` on its own connection, to be read as it arrives.
fn open_stream(server: &Server, path: &str) -> TcpStream {
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                server.port
            )
            .as_bytes(),
        )
        .unwrap();
    stream
}

/// Reads `stream` into `bytes` until `marker` is in them: true, or false at
/// the end of the response or after `within`.
fn read_until(stream: &mut TcpStream, bytes: &mut Vec<u8>, marker: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    let mut buf = [0u8; 4096];
    while !String::from_utf8_lossy(bytes).contains(marker) {
        if Instant::now() >= deadline {
            return false;
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return false,
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
        }
    }
    true
}

/// One request for the stream, read as it arrives: when the first chunk's
/// text was in hand, when the second's was, and the whole response.
fn read_stream(server: &Server) -> (Duration, Duration, String) {
    let mut stream = open_stream(server, "/api/stream");
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

// A respawn (the runner replaced once enough module instances went stale;
// here after the first reload, with `OJ_START_RESPAWN_STALE=1`) takes the
// engine's write lock. A body pulled through that lock held it until the
// handler's next chunk, so an idle event feed held the respawn up, and every
// request queued behind the waiting writer (tokio's read-write lock is
// write-preferring) until the feed wrote again. A body holds its runner
// instead: the respawn goes through at once, the new runner answers, and the
// feed finishes on the runner that started it, as a module graph swap under
// Vite never stops a handler already running.
#[test]
fn a_stream_in_flight_does_not_hold_up_the_runners_respawn() {
    let Some(app) = app("streaming-respawn") else {
        return;
    };
    let root = &app.0;
    write_app(root);
    let dev = Server::oj_dev_with(root, &[("OJ_START_RESPAWN_STALE", "1")]);
    let (status, _) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "the page\n{}", dev.output());

    // A feed that writes once, then idles.
    let mut feed = open_stream(&dev, &format!("/api/stream?gap={}", IDLE.as_millis()));
    let mut bytes = Vec::new();
    assert!(
        read_until(&mut feed, &mut bytes, "first-chunk", WAIT),
        "the feed's first chunk never came\n{}",
        dev.output()
    );

    // A rendered route edited: the reload drops its module, the one stale
    // instance the threshold asks for, and the runner respawns.
    let about = root.join("src/routes/about.tsx");
    let source = std::fs::read_to_string(&about).unwrap();
    std::fs::write(&about, format!("{source}\n// respawn probe\n")).unwrap();
    let edited = Instant::now();
    let budget = IDLE / 2;
    while edited.elapsed() < budget && !dev.output().contains("engine respawned") {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        dev.output().contains("engine respawned"),
        "no respawn within {budget:?} of the edit: the idle feed's pull held the engine's lock\n{}",
        dev.output()
    );

    // The new runner answers while the feed still idles.
    let (status, body) = dev.request("GET", "/api/stream?gap=0", &[]);
    assert!(
        status == 200 && body.contains("second-chunk") && edited.elapsed() < IDLE,
        "the respawned runner answered {status} after {:?}: {body:?}\n{}",
        edited.elapsed(),
        dev.output()
    );

    // The feed opened before the respawn finishes on the runner that started it.
    assert!(
        read_until(&mut feed, &mut bytes, "second-chunk", WAIT),
        "the feed never finished after the respawn\n{}\n{}",
        String::from_utf8_lossy(&bytes),
        dev.output()
    );
}
