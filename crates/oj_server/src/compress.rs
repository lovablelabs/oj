use super::*;
use tower_http::compression::predicate::SizeAbove;
use tower_http::compression::{CompressionLayer, CompressionLevel, Predicate};

/// Text a browser decodes before it uses it: scripts, styles, pages, JSON
/// (a source map), SVG, WebAssembly. Media is already compressed, and a
/// compressed media response has no length, which breaks the range requests
/// a `<video>` seeks with.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Compressible;

impl Predicate for Compressible {
    fn should_compress<B: axum::body::HttpBody>(&self, response: &axum::http::Response<B>) -> bool {
        let mime = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| {
                ct.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
            })
            .unwrap_or_default();
        (mime.starts_with("text/") && mime != "text/event-stream")
            || matches!(
                mime.as_str(),
                "application/javascript"
                    | "application/json"
                    | "application/manifest+json"
                    | "application/wasm"
                    | "application/xml"
                    | "image/svg+xml"
            )
    }
}

/// Below this many bytes a gzip header and the CPU cost no round trip's
/// worth: a 304, a stub module, a `?url` asset module.
pub(crate) const MIN_COMPRESSED_BYTES: u16 = 1024;

/// Gzip for every text response a client accepts it for, at the fastest
/// level: a dev module's bytes are mostly identifiers and whitespace, which
/// level 1 cuts to a third. Vite's dev server compresses nothing (its preview
/// server does); over a preview's real network the transfer of a thousand
/// uncompressed modules is a visible part of the load, so oj compresses.
pub(crate) fn layer(
) -> CompressionLayer<tower_http::compression::predicate::And<SizeAbove, Compressible>> {
    CompressionLayer::new()
        .quality(CompressionLevel::Fastest)
        .compress_when(SizeAbove::new(MIN_COMPRESSED_BYTES).and(Compressible))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn says(content_type: Option<&str>) -> bool {
        let mut b = axum::http::Response::builder();
        if let Some(ct) = content_type {
            b = b.header(header::CONTENT_TYPE, ct);
        }
        Compressible.should_compress(&b.body(axum::body::Body::empty()).unwrap())
    }

    #[test]
    fn text_and_json_compress_media_and_streams_do_not() {
        for ct in [
            "text/javascript",
            "text/javascript; charset=utf-8",
            "text/css",
            "text/html",
            "application/json",
            "image/svg+xml",
            "application/wasm",
        ] {
            assert!(says(Some(ct)), "{ct} should compress");
        }
        for ct in [
            "image/png",
            "video/mp4",
            "font/woff2",
            "application/octet-stream",
            "text/event-stream",
        ] {
            assert!(!says(Some(ct)), "{ct} should not compress");
        }
        assert!(!says(None), "an untyped response is left alone");
    }
}
