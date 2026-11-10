// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A `publicDir` file's response, as Vite's servePublicMiddleware answers
//! through sirv (`dev: true, etag: true`): the file's bytes as they are, its
//! content type (the TypeScript extensions as JavaScript, sirv's
//! `setHeaders` in Vite's static.ts), a weak `W/"<size>-<mtime ms>"` ETag
//! answered with 304, `Last-Modified`, `Cache-Control: no-cache`, and byte
//! ranges (206 with `Content-Range`, 416 past the end), which a browser
//! needs to play or seek a video. The body is streamed from the file, as
//! sirv pipes a read stream, never held whole in memory.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};

/// The content type of a public file by its extension (any case): Vite's
/// `knownJavascriptExtensionRE` types are JavaScript (`.ts` is registered as
/// video/mp2t), the rest come from the table the dev server shares.
pub fn public_content_type(ext: &str) -> &'static str {
    let ext = ext.to_ascii_lowercase();
    match ext.as_str() {
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "mts" | "cts" => "text/javascript",
        "htm" => "text/html; charset=utf-8",
        other => crate::util::content_type(other),
    }
}

/// What a `Range` header asks of a file of `size` bytes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ByteRange {
    /// No usable range (absent, another unit, several ranges, malformed):
    /// the whole file, 200.
    Whole,
    /// `start..=end`, both inside the file: 206.
    Part(u64, u64),
    /// Starts past the end: 416.
    Unsatisfiable,
}

/// One `bytes=` range (RFC 9110 14.1.2): `a-b`, `a-` (to the end) and `-n`
/// (the last n bytes), the end clamped to the file. sirv reads `a-b` and `a-`
/// the same way and answers 416 for a start past the end.
pub(crate) fn byte_range(value: &str, size: u64) -> ByteRange {
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return ByteRange::Whole;
    };
    if spec.contains(',') {
        return ByteRange::Whole;
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return ByteRange::Whole;
    };
    let (first, last) = (first.trim(), last.trim());
    let number = |s: &str| s.parse::<u64>().ok();
    match (first.is_empty(), last.is_empty()) {
        (true, true) => ByteRange::Whole,
        // The last n bytes.
        (true, false) => match number(last) {
            None => ByteRange::Whole,
            Some(0) => ByteRange::Unsatisfiable,
            Some(_) if size == 0 => ByteRange::Unsatisfiable,
            Some(n) => ByteRange::Part(size.saturating_sub(n), size - 1),
        },
        (false, _) => {
            let Some(start) = number(first) else {
                return ByteRange::Whole;
            };
            if start >= size {
                return ByteRange::Unsatisfiable;
            }
            let end = if last.is_empty() {
                size - 1
            } else {
                match number(last) {
                    Some(end) if end >= start => end.min(size - 1),
                    _ => return ByteRange::Whole,
                }
            };
            ByteRange::Part(start, end)
        }
    }
}

/// `time` as an HTTP date (RFC 9110 5.6.7): `Sun, 06 Nov 1994 08:49:37 GMT`.
pub(crate) fn http_date(time: SystemTime) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAYS[(days % 7) as usize],
        day,
        MONTHS[(month - 1) as usize],
        year,
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

/// How much of the file one read takes.
const CHUNK: u64 = 256 * 1024;

/// `len` bytes of `file` from `start`, read as the response is written.
async fn file_body(file: &Path, start: u64, len: u64) -> std::io::Result<Body> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut handle = tokio::fs::File::open(file).await?;
    if start > 0 {
        handle.seek(std::io::SeekFrom::Start(start)).await?;
    }
    let stream = futures_util::stream::unfold((handle, len), |(mut handle, left)| async move {
        if left == 0 {
            return None;
        }
        let mut buf = vec![0u8; left.min(CHUNK) as usize];
        match handle.read(&mut buf).await {
            // The file shrank under the response: end it short.
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((
                    Ok::<_, std::io::Error>(Bytes::from(buf)),
                    (handle, left - n as u64),
                ))
            }
            Err(e) => Some((Err(e), (handle, 0))),
        }
    });
    Ok(Body::from_stream(stream))
}

/// The response for the public file at `file` (a regular file, `meta` its
/// metadata) to a GET or HEAD carrying `request` headers. See the module doc.
pub async fn public_file_response(
    file: &Path,
    meta: &std::fs::Metadata,
    method: &Method,
    request: &HeaderMap,
) -> Response {
    let size = meta.len();
    let modified = meta.modified().ok();
    let millis = modified
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    // sirv's validator: `W/"<size>-<mtime ms>"`.
    let etag = format!("W/\"{size}-{millis}\"");
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, v);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if crate::etag::not_modified(request, &etag) {
        return (StatusCode::NOT_MODIFIED, headers).into_response();
    }
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(public_content_type(ext)),
    );
    if let Some(v) = modified.and_then(|t| HeaderValue::from_str(&http_date(t)).ok()) {
        headers.insert(header::LAST_MODIFIED, v);
    }
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let range = request
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map_or(ByteRange::Whole, |v| byte_range(v, size));
    let (status, start, len) = match range {
        ByteRange::Whole => (StatusCode::OK, 0, size),
        ByteRange::Part(start, end) => {
            if let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{end}/{size}")) {
                headers.insert(header::CONTENT_RANGE, v);
            }
            (StatusCode::PARTIAL_CONTENT, start, end - start + 1)
        }
        ByteRange::Unsatisfiable => {
            if let Ok(v) = HeaderValue::from_str(&format!("bytes */{size}")) {
                headers.insert(header::CONTENT_RANGE, v);
            }
            return (StatusCode::RANGE_NOT_SATISFIABLE, headers).into_response();
        }
    };
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    if *method == Method::HEAD || len == 0 {
        return (status, headers, Body::empty()).into_response();
    }
    match file_body(file, start, len).await {
        Ok(body) => (status, headers, body).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("oj: public file: {e}"),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_ranges_follow_the_spec_and_sirv() {
        use ByteRange::*;
        assert_eq!(byte_range("bytes=0-99", 4000), Part(0, 99));
        assert_eq!(byte_range("bytes=100-", 4000), Part(100, 3999));
        // The end is clamped to the file (sirv: `if (end >= stats.size)`).
        assert_eq!(byte_range("bytes=3990-9999", 4000), Part(3990, 3999));
        // The last n bytes.
        assert_eq!(byte_range("bytes=-500", 4000), Part(3500, 3999));
        assert_eq!(byte_range("bytes=-9999", 4000), Part(0, 3999));
        // A start past the end: 416 (sirv: `if (start >= stats.size)`).
        assert_eq!(byte_range("bytes=4000-", 4000), Unsatisfiable);
        assert_eq!(byte_range("bytes=0-", 0), Unsatisfiable);
        assert_eq!(byte_range("bytes=-0", 4000), Unsatisfiable);
        // Not a single byte range: the whole file.
        assert_eq!(byte_range("items=0-9", 4000), Whole);
        assert_eq!(byte_range("bytes=0-1,5-9", 4000), Whole);
        assert_eq!(byte_range("bytes=9-1", 4000), Whole);
        assert_eq!(byte_range("bytes=a-b", 4000), Whole);
        assert_eq!(byte_range("bytes=-", 4000), Whole);
    }

    #[test]
    fn http_dates_are_imf_fixdate() {
        let at = |secs: u64| http_date(UNIX_EPOCH + std::time::Duration::from_secs(secs));
        assert_eq!(at(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        // RFC 9110's own example.
        assert_eq!(at(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
        // A leap day.
        assert_eq!(at(1_709_210_096), "Thu, 29 Feb 2024 12:34:56 GMT");
        assert_eq!(at(951_782_400), "Tue, 29 Feb 2000 00:00:00 GMT");
    }

    #[test]
    fn typescript_extensions_are_javascript() {
        // Vite's knownJavascriptExtensionRE: /\.(?:[tj]sx?|[cm][tj]s)$/.
        for ext in ["js", "jsx", "ts", "tsx", "mjs", "cjs", "mts", "cts", "TS"] {
            assert_eq!(public_content_type(ext), "text/javascript", "{ext}");
        }
        assert_eq!(public_content_type("html"), "text/html; charset=utf-8");
        assert_eq!(public_content_type("htm"), "text/html; charset=utf-8");
        assert_eq!(
            public_content_type("webmanifest"),
            "application/manifest+json"
        );
        assert_eq!(public_content_type("mov"), "video/quicktime");
        assert_eq!(public_content_type(""), "application/octet-stream");
    }

    async fn body_of(response: Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    fn with(name: header::HeaderName, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, HeaderValue::from_str(value).unwrap());
        h
    }

    // sirv's answers for one file: the whole file with its validators, a
    // range of it, a range past its end, a matching validator, and a HEAD.
    #[tokio::test]
    async fn a_public_file_answers_whole_ranged_and_conditional_requests() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("clip.mp4");
        // Longer than one read, so a body spans chunks.
        let bytes: Vec<u8> = (0..(CHUNK as usize + 1000))
            .map(|i| (i % 251) as u8)
            .collect();
        std::fs::write(&file, &bytes).unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        let size = bytes.len();

        let whole = public_file_response(&file, &meta, &Method::GET, &HeaderMap::new()).await;
        assert_eq!(whole.status(), StatusCode::OK);
        let h = whole.headers().clone();
        assert_eq!(h[header::CONTENT_TYPE], "video/mp4");
        assert_eq!(h[header::CONTENT_LENGTH], size.to_string().as_str());
        assert_eq!(h[header::ACCEPT_RANGES], "bytes");
        assert_eq!(h[header::CACHE_CONTROL], "no-cache");
        let etag = h[header::ETAG].to_str().unwrap().to_string();
        assert!(
            etag.starts_with(&format!("W/\"{size}-")),
            "sirv's W/\"<size>-<mtime>\": {etag}"
        );
        assert!(h[header::LAST_MODIFIED].to_str().unwrap().ends_with(" GMT"));
        assert_eq!(body_of(whole).await, bytes);

        let part = public_file_response(
            &file,
            &meta,
            &Method::GET,
            &with(header::RANGE, "bytes=10-19"),
        )
        .await;
        assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            part.headers()[header::CONTENT_RANGE],
            format!("bytes 10-19/{size}").as_str()
        );
        assert_eq!(part.headers()[header::CONTENT_LENGTH], "10");
        assert_eq!(body_of(part).await, &bytes[10..20]);

        // A range crossing a read boundary, to the end of the file.
        let from = CHUNK as usize - 5;
        let tail = public_file_response(
            &file,
            &meta,
            &Method::GET,
            &with(header::RANGE, &format!("bytes={from}-")),
        )
        .await;
        assert_eq!(tail.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(body_of(tail).await, &bytes[from..]);

        let past = public_file_response(
            &file,
            &meta,
            &Method::GET,
            &with(header::RANGE, &format!("bytes={size}-")),
        )
        .await;
        assert_eq!(past.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            past.headers()[header::CONTENT_RANGE],
            format!("bytes */{size}").as_str()
        );

        let same = public_file_response(
            &file,
            &meta,
            &Method::GET,
            &with(header::IF_NONE_MATCH, &etag),
        )
        .await;
        assert_eq!(same.status(), StatusCode::NOT_MODIFIED);
        assert!(body_of(same).await.is_empty());

        let head = public_file_response(&file, &meta, &Method::HEAD, &HeaderMap::new()).await;
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(
            head.headers()[header::CONTENT_LENGTH],
            size.to_string().as_str()
        );
        assert!(body_of(head).await.is_empty());
    }
}
