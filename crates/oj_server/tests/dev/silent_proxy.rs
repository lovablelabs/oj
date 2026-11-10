// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A TCP proxy that leaves WebSocket connections half-open on demand, shared
//! by the dev socket keepalive tests (oj_server's SPA client, oj's Start
//! client).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A TCP proxy to oj that can leave its open WebSocket connections
/// half-open: [`SilentProxy::silence`] makes each one drop what either end
/// sends and never close, as a connection lost on the way looks to both
/// ends. Plain HTTP connections always pass: the page's reload must work.
pub struct SilentProxy {
    pub port: u16,
    /// Bumped by `silence`: a socket opened under an older one is dead.
    generation: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for SilentProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SilentProxy {
    pub async fn start(upstream: u16) -> SilentProxy {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let generation = Arc::new(AtomicU64::new(0));
        let task = tokio::spawn({
            let generation = Arc::clone(&generation);
            async move {
                while let Ok((client, _)) = listener.accept().await {
                    tokio::spawn(relay(client, upstream, Arc::clone(&generation)));
                }
            }
        });
        SilentProxy {
            port,
            generation,
            task,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Every WebSocket open now goes silent, both ways, for good.
    pub fn silence(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
}

async fn relay(mut client: TcpStream, upstream: u16, generation: Arc<AtomicU64>) {
    let Ok(mut server) = TcpStream::connect(("127.0.0.1", upstream)).await else {
        return;
    };
    // The request's head says whether this connection is a WebSocket.
    let mut first = vec![0u8; 16 * 1024];
    let n = match client.read(&mut first).await {
        Ok(0) | Err(_) => return,
        Ok(n) => n,
    };
    let head = String::from_utf8_lossy(&first[..n]).to_ascii_lowercase();
    let socket = head.contains("upgrade: websocket");
    if server.write_all(&first[..n]).await.is_err() {
        return;
    }
    let born = generation.load(Ordering::SeqCst);
    let alive: Arc<dyn Fn() -> bool + Send + Sync> = if socket {
        let generation = Arc::clone(&generation);
        Arc::new(move || generation.load(Ordering::SeqCst) == born)
    } else {
        Arc::new(|| true)
    };
    let (client_read, client_write) = client.into_split();
    let (server_read, server_write) = server.into_split();
    tokio::spawn(pipe(client_read, server_write, Arc::clone(&alive)));
    tokio::spawn(pipe(server_read, client_write, alive));
}

/// Copies `from` to `to` while `alive`; after, swallows what arrives and
/// holds `to` open forever: no byte, no close, no reset.
async fn pipe(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    alive: Arc<dyn Fn() -> bool + Send + Sync>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if !alive() {
            continue;
        }
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
    if alive() {
        let _ = to.shutdown().await;
    } else {
        // The other end must not learn the connection is gone.
        let _hold = (from, to);
        std::future::pending::<()>().await;
    }
}
