use super::*;

fn opts(json: &str) -> oj_config::ProxyOptions {
    serde_json::from_str(json).unwrap()
}

fn reply(status: u16, location: &str, cookies: &[&str]) -> (u16, HeaderMap) {
    let mut headers = HeaderMap::new();
    headers.insert(header::LOCATION, location.parse().unwrap());
    for c in cookies {
        headers.append(header::SET_COOKIE, c.parse().unwrap());
    }
    (status, headers)
}

fn cookies(headers: &HeaderMap) -> Vec<&str> {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect()
}

// http-proxy-3 setRedirectHostRewrite: only a redirect (201/301/302/307/308)
// whose Location is on the target's host moves, and only when asked.
#[test]
fn location_rewrites_follow_http_proxy() {
    let auto = opts(r#"{"target":"http://127.0.0.1:9000","autoRewrite":true}"#);
    let (status, mut h) = reply(302, "http://127.0.0.1:9000/next?x=1", &[]);
    rewrite_reply_headers(&auto, Some("localhost:5173"), status, &mut h);
    assert_eq!(h[header::LOCATION], "http://localhost:5173/next?x=1");

    let host = opts(
        r#"{"target":"http://127.0.0.1:9000","hostRewrite":"app.test","protocolRewrite":"https"}"#,
    );
    let (status, mut h) = reply(307, "http://127.0.0.1:9000/next", &[]);
    rewrite_reply_headers(&host, Some("localhost:5173"), status, &mut h);
    // The host setter keeps the old port when the value names none.
    assert_eq!(h[header::LOCATION], "https://app.test:9000/next");

    for (status, location) in [
        (302, "http://elsewhere.test/next"), // another host
        (302, "/next"),                      // relative
        (303, "http://127.0.0.1:9000/next"), // 303 is not in redirectRegex
        (200, "http://127.0.0.1:9000/next"),
    ] {
        let (status, mut h) = reply(status, location, &[]);
        rewrite_reply_headers(&auto, Some("localhost:5173"), status, &mut h);
        assert_eq!(h[header::LOCATION], location, "{status} {location}");
    }

    // Without an option nothing moves.
    let plain = opts(r#"{"target":"http://127.0.0.1:9000"}"#);
    let (status, mut h) = reply(302, "http://127.0.0.1:9000/next", &["a=1; Domain=x.test"]);
    rewrite_reply_headers(&plain, Some("localhost:5173"), status, &mut h);
    assert_eq!(h[header::LOCATION], "http://127.0.0.1:9000/next");
    assert_eq!(cookies(&h), ["a=1; Domain=x.test"]);
}

// http-proxy-3 rewriteCookieProperty, on every Set-Cookie.
#[test]
fn cookie_rewrites_follow_http_proxy() {
    let all = opts(
        r#"{"target":"http://t","cookieDomainRewrite":"localhost","cookiePathRewrite":{"/api":"/","/keep":"/keep"}}"#,
    );
    let (status, mut h) = reply(
        200,
        "/",
        &[
            "a=1; Domain=backend.test; Path=/api; HttpOnly",
            "b=2; path=/other",
            "c=3",
        ],
    );
    rewrite_reply_headers(&all, None, status, &mut h);
    assert_eq!(
        cookies(&h),
        [
            "a=1; Domain=localhost; Path=/; HttpOnly",
            "b=2; path=/other",
            "c=3"
        ]
    );

    // A map entry (or "*") with an empty value removes the attribute.
    let remove =
        opts(r#"{"target":"http://t","cookieDomainRewrite":{"backend.test":"","*":"kept.test"}}"#);
    let (status, mut h) = reply(
        200,
        "/",
        &["a=1; Domain=backend.test; Path=/", "b=2; domain=other.test"],
    );
    rewrite_reply_headers(&remove, None, status, &mut h);
    assert_eq!(cookies(&h), ["a=1; Path=/", "b=2; domain=kept.test"]);

    let off = opts(r#"{"target":"http://t","cookieDomainRewrite":false}"#);
    let (status, mut h) = reply(200, "/", &["a=1; Domain=backend.test"]);
    rewrite_reply_headers(&off, None, status, &mut h);
    assert_eq!(cookies(&h), ["a=1; Domain=backend.test"]);
}

// Vite: a string entry is `{ target, changeOrigin: true }`.
#[test]
fn string_entries_change_origin_like_vite() {
    let entries: std::collections::BTreeMap<String, oj_config::ProxyEntry> = serde_json::from_str(
        r#"{"/a":"http://t","/b":{"target":"http://t"},"/c":{"target":"http://t","changeOrigin":true}}"#,
    )
    .unwrap();
    assert!(entries["/a"].change_origin());
    assert!(!entries["/b"].change_origin());
    assert!(entries["/c"].change_origin());
}

/// An upstream for the redirect rules: `/r/<status>` redirects to
/// `/echo` on `next_host`; `/echo` answers with the method, body and cookie
/// it received.
async fn redirect_server(next_host: fn(u16) -> String) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new()
        .route(
            "/r/{status}",
            axum::routing::any(
                move |axum::extract::Path(status): axum::extract::Path<u16>| async move {
                    Response::builder()
                        .status(status)
                        .header(header::LOCATION, format!("http://{}/echo", next_host(port)))
                        .body(Body::empty())
                        .unwrap()
                },
            ),
        )
        .route(
            "/loop",
            axum::routing::any(|| async {
                Response::builder()
                    .status(302)
                    .header(header::LOCATION, "/loop")
                    .body(Body::empty())
                    .unwrap()
            }),
        )
        .route(
            "/echo",
            axum::routing::any(
                |method: Method, headers: HeaderMap, body: String| async move {
                    let cookie = headers
                        .get(header::COOKIE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("-")
                        .to_string();
                    format!("{method} {body} {cookie}")
                },
            ),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

async fn follow(port: u16, method: Method, path: &str) -> Result<(u16, String), String> {
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, "sid=1".parse().unwrap());
    headers.insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
    let resp = follow_redirects(
        &plugin_mw::browser_hop_client(false),
        method,
        &format!("http://127.0.0.1:{port}{path}"),
        headers,
        axum::body::Bytes::from_static(b"payload"),
    )
    .await?;
    Ok((resp.status().as_u16(), resp.text().await.unwrap()))
}

// follow-redirects' rules, which http-proxy's `followRedirects` uses.
#[tokio::test]
async fn follow_redirects_keeps_follow_redirects_rules() {
    let port = redirect_server(|port| format!("127.0.0.1:{port}")).await;
    // 307/308 replay the method and the body; 301/302 keep a PUT.
    for (method, status, want) in [
        (Method::POST, 307, "POST payload sid=1"),
        (Method::PUT, 308, "PUT payload sid=1"),
        (Method::PUT, 302, "PUT payload sid=1"),
        // A POST answered with 301/302, and anything answered with 303,
        // continues as a body-less GET.
        (Method::POST, 302, "GET  sid=1"),
        (Method::DELETE, 303, "GET  sid=1"),
    ] {
        let got = follow(port, method.clone(), &format!("/r/{status}"))
            .await
            .unwrap();
        assert_eq!(got, (200, want.to_string()), "{method} {status}");
    }
    assert_eq!(
        follow(port, Method::GET, "/loop").await.unwrap_err(),
        "Maximum number of redirects exceeded"
    );

    // Leaving the host drops the cookie (the body stays: the method did not
    // change).
    let port = redirect_server(|port| format!("localhost:{port}")).await;
    assert_eq!(
        follow(port, Method::GET, "/r/302").await.unwrap(),
        (200, "GET payload -".to_string())
    );
}

// http-proxy-3's XHeaders, from the browser's connection.
#[tokio::test]
async fn forwarded_headers_follow_http_proxy() {
    let conn = crate::tls::ClientConn {
        addr: "203.0.113.7:5555".parse().unwrap(),
        encrypted: true,
    };
    let mut h = HeaderMap::new();
    h.insert(header::HOST, "localhost:8443".parse().unwrap());
    h.append("x-forwarded-for", "10.0.0.1".parse().unwrap());
    h.append("x-forwarded-for", "10.0.0.2".parse().unwrap());
    crate::tls::with_client_conn(conn, async { add_forwarded_headers(&mut h, false) }).await;
    assert_eq!(h["x-forwarded-for"], "10.0.0.1, 10.0.0.2,203.0.113.7");
    assert_eq!(h["x-forwarded-port"], "8443");
    assert_eq!(h["x-forwarded-proto"], "https");
    assert_eq!(h["x-forwarded-host"], "localhost:8443");

    // A WebSocket: ws/wss, no x-forwarded-host; no port in the Host means
    // the scheme's.
    let mut h = HeaderMap::new();
    h.insert(header::HOST, "app.test".parse().unwrap());
    let plain = crate::tls::ClientConn {
        encrypted: false,
        ..conn
    };
    crate::tls::with_client_conn(plain, async { add_forwarded_headers(&mut h, true) }).await;
    assert_eq!(h["x-forwarded-for"], "203.0.113.7");
    assert_eq!(h["x-forwarded-port"], "80");
    assert_eq!(h["x-forwarded-proto"], "ws");
    assert!(!h.contains_key("x-forwarded-host"));
}

// The WebSocket proxy applies `xfwd` to the upgrade it sends the target.
#[tokio::test]
async fn websocket_upgrade_carries_forwarded_headers_with_xfwd() {
    let entry: oj_config::ProxyEntry =
        serde_json::from_str(r#"{"target":"ws://127.0.0.1:9","ws":true,"xfwd":true}"#).unwrap();
    let mut h = HeaderMap::new();
    h.insert(header::HOST, "localhost:5173".parse().unwrap());
    let conn = crate::tls::ClientConn {
        addr: "[2001:db8::1]:4000".parse().unwrap(),
        encrypted: true,
    };
    let req = crate::tls::with_client_conn(conn, async {
        upstream_ws_request("ws://127.0.0.1:9/socket", &h, Some(&entry)).unwrap()
    })
    .await;
    assert_eq!(req.headers()["x-forwarded-for"], "2001:db8::1");
    assert_eq!(req.headers()["x-forwarded-port"], "5173");
    assert_eq!(req.headers()["x-forwarded-proto"], "wss");
}

// The loopback hop carries the connection and drops a client's copy.
#[tokio::test]
async fn loopback_hop_carries_the_connection_privately() {
    let mut h = HeaderMap::new();
    h.insert(header::HOST, "localhost:5173".parse().unwrap());
    h.insert("x-oj-remote-address", "6.6.6.6".parse().unwrap());
    h.insert("x-oj-encrypted", "1".parse().unwrap());
    let spoofed = plugin_mw::loopback_request_headers(&h);
    assert!(
        !spoofed
            .iter()
            .any(|(n, _)| n.as_str() == "x-oj-remote-address" || n.as_str() == "x-oj-encrypted"),
        "{spoofed:?}"
    );
    let conn = crate::tls::ClientConn {
        addr: "203.0.113.7:5555".parse().unwrap(),
        encrypted: false,
    };
    let carried =
        crate::tls::with_client_conn(conn, async { plugin_mw::loopback_request_headers(&h) }).await;
    let get = |name: &str| {
        carried
            .iter()
            .filter(|(n, _)| n.as_str() == name)
            .map(|(_, v)| v.to_str().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(get("x-oj-remote-address"), ["203.0.113.7"]);
    assert!(get("x-oj-encrypted").is_empty());
}
