// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! TLS for the dev servers, the way Vite's `resolveHttpServer` does it: with
//! `server.https` configured, Vite serves `node:http2.createSecureServer`
//! with `allowHTTP1` — ALPN-negotiated h2 with an http/1.1 fallback — and
//! plain HTTP otherwise. Multiplexing is the point: a dev page is thousands
//! of module requests, and over a real network h2 collapses the per-request
//! round-trip serialization that six http/1.1 connections pay.
//!
//! Vite drops to https/1.1 when `server.proxy` is set, a node limitation
//! (its proxy library cannot answer h2 server requests); the axum proxy has
//! no such limit, so h2 stays on.

use std::sync::Arc;

use hyper_util::rt::{TokioExecutor, TokioIo};

/// Serve the router on the listener: plain HTTP without `server.https`, and
/// rustls with ALPN h2 + http/1.1 with it. WebSockets ride http/1.1: the h2
/// SETTINGS never advertise extended CONNECT (RFC 8441), exactly like node's
/// http2 server under Vite, so a browser opens its wss connection with ALPN
/// http/1.1 and the ordinary Upgrade path answers it.
pub async fn serve_router(
    listener: tokio::net::TcpListener,
    router: axum::Router,
    https: Option<&oj_config::HttpsConfig>,
) -> anyhow::Result<()> {
    let Some(https) = https else {
        axum::serve(listener, router).await?;
        return Ok(());
    };
    let acceptor = tokio_rustls::TlsAcceptor::from(tls_config(https)?);
    loop {
        // axum::serve's accept handling: a peer gone between SYN and accept
        // ends nothing but its own connection, and anything else (EMFILE
        // under fd pressure) logs and backs off — a failed accept never
        // exits the dev server, exactly as on the plain-http path.
        let (tcp, _remote) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                use std::io::ErrorKind;
                if !matches!(
                    e.kind(),
                    ErrorKind::ConnectionRefused
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::ConnectionReset
                ) {
                    eprintln!("accept error: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let router = router.clone();
        tokio::spawn(async move {
            // A failed handshake (a plain-http probe, a cipher mismatch) only
            // ends that connection, as a bad TLS client does on any server.
            let Ok(stream) = acceptor.accept(tcp).await else {
                return;
            };
            let service = hyper_util::service::TowerToHyperService::new(router);
            let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// The URL scheme the dev server answers on, for printed URLs and `open`.
pub fn url_scheme(https: Option<&oj_config::HttpsConfig>) -> &'static str {
    if https.is_some() {
        "https"
    } else {
        "http"
    }
}

fn tls_config(https: &oj_config::HttpsConfig) -> anyhow::Result<Arc<rustls::ServerConfig>> {
    let certs: Vec<_> = rustls_pemfile::certs(&mut https.cert.as_bytes())
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("server.https.cert is not readable PEM: {e}"))?;
    if certs.is_empty() {
        anyhow::bail!("server.https.cert holds no certificate");
    }
    let key = rustls_pemfile::private_key(&mut https.key.as_bytes())
        .map_err(|e| anyhow::anyhow!("server.https.key is not readable PEM: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("server.https.key holds no private key"))?;
    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("server.https key/cert rejected: {e}"))?;
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}
