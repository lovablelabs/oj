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

// `followRedirects: true` follows, the default client never does.
#[tokio::test]
async fn following_client_follows_and_the_default_does_not() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new()
        .route(
            "/out",
            axum::routing::any(|| async {
                Response::builder()
                    .status(302)
                    .header(header::LOCATION, "/next")
                    .body(Body::empty())
                    .unwrap()
            }),
        )
        .route("/next", axum::routing::any(|| async { "followed" }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("http://127.0.0.1:{port}/out");
    for secure in [true, false] {
        let followed = following_client(secure).get(&url).send().await.unwrap();
        assert_eq!(followed.status(), 200);
        assert_eq!(followed.text().await.unwrap(), "followed");
        let kept = plugin_mw::browser_hop_client(!secure)
            .get(&url)
            .send()
            .await
            .unwrap();
        assert_eq!(kept.status(), 302);
    }
    server.abort();
}
