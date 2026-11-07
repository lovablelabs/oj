// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Conditional-request validators, shared by the module server and Start dev.

use axum::http::{header, HeaderMap};

/// Weak: the plain and gzip bodies of one representation share it, and a
/// strong validator must differ between content codings (RFC 9110 8.8.1).
pub fn weak_etag(hash: &str) -> String {
    format!("W/\"{hash}\"")
}

/// Whether the request's `If-None-Match` names `etag`, by the weak comparison
/// `If-None-Match` uses (RFC 9110 13.1.2): a list of tags, `*` matching any,
/// and `W/` ignored on both sides.
pub fn not_modified(req: &HeaderMap, etag: &str) -> bool {
    let opaque = |t: &str| {
        let t = t.trim();
        t.strip_prefix("W/").unwrap_or(t).to_string()
    };
    let ours = opaque(etag);
    req.get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == "*" || opaque(t) == ours))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn inm(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_str(v).unwrap());
        h
    }

    #[test]
    fn if_none_match_compares_weakly() {
        let weak = weak_etag("abc123");
        assert!(not_modified(&inm("\"abc123\""), &weak));
        assert!(not_modified(&inm("\"x\", W/\"abc123\""), &weak));
        assert!(not_modified(&inm("*"), &weak));
        assert!(!not_modified(&inm("W/\"abc124\""), &weak));
        assert!(!not_modified(&HeaderMap::new(), &weak));

        let strong = "\"abc123\"";
        assert!(not_modified(&inm("W/\"abc123\""), strong));
        assert!(not_modified(&inm("\"a\", \"abc123\", \"b\""), strong));
        assert!(!not_modified(&inm("\"abc\""), strong));
    }
}
