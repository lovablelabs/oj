// session.go handleBatchRequests / paginateBatchResponses / handleBatchRequest and proto.go
// BatchRequestsResponse.MarshalJSONTo. Responses are kept as encoded JSON text so pages are cut on exact
// byte lengths like Go.

use tsrs_core::json::{self, Value};

use crate::handler::{ApiError, ApiResult, Response};
use crate::session::Session;
use crate::wire::Params;

/// Go `DefaultMaxResponseBytesPerPage`.
const DEFAULT_MAX_RESPONSE_BYTES_PER_PAGE: usize = 300_000_000;

const SOURCE_FILE_RESPONSE_METHODS: &[&str] = &[
    "createSourceFile",
    "createSourceFileFromFile",
    "getSourceFile",
    "getCachedSourceFile",
    "getConfigSourceFile",
    "typeToTypeNode",
    "signatureToSignatureDeclaration",
];

pub fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    out.extend(data.chunks(3).flat_map(|chunk| {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        [
            T[(n >> 18) as usize & 63] as char,
            T[(n >> 12) as usize & 63] as char,
            if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' },
            if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' },
        ]
    }));
    out
}

/// Go `base64.StdEncoding.DecodeString` (padding required, no whitespace).
pub fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    let b = text.as_bytes();
    if !b.len().is_multiple_of(4) {
        return Err(format!("illegal base64 data at input byte {}", b.len() - b.len() % 4));
    }
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(b.len() / 4 * 3);
    for (ci, chunk) in b.chunks(4).enumerate() {
        let last = ci == b.len() / 4 - 1;
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return Err(format!("illegal base64 data at input byte {}", ci * 4 + 4 - pad));
        }
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            let v = if i >= 4 - pad {
                0
            } else {
                val(c).ok_or_else(|| format!("illegal base64 data at input byte {}", ci * 4 + i))?
            };
            n = (n << 6) | v;
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

fn encode_batch_response(method: &str, result: &str, error: Option<&str>) -> String {
    let mut out = String::from("{\"method\":");
    out.push_str(&json::marshal_string(method));
    out.push_str(",\"result\":");
    out.push_str(result);
    if let Some(e) = error
        && !e.is_empty()
    {
        out.push_str(",\"error\":");
        out.push_str(&json::marshal_string(e));
    }
    out.push('}');
    out
}

impl Session {
    /// Go `handleBatchRequest`.
    fn handle_batch_request(&self, request: &Value, raw_params: Option<Vec<u8>>) -> String {
        let p = Params(request);
        let method = match p.get("method") {
            Value::String(m) => m.clone(),
            _ => String::new(),
        };
        if method == "batchRequests" {
            return encode_batch_response(
                &method,
                "null",
                Some("api: invalid request: batchRequests cannot be nested"),
            );
        }
        // Go keeps each item's params as raw `json.Value` bytes: re-encoding the parsed value would lose number
        // literals (`1e3`, integers above 2^53) and so change decoding.
        let params = match p.get("params") {
            Value::Null => Vec::new(),
            v => raw_params.unwrap_or_else(|| json::marshal(v).unwrap_or_default().into_bytes()),
        };
        // `handle_request` already turns panics into errors.
        match crate::handler::Handler::handle_request(self, &method, &params) {
            Ok(Response::Json(text)) => encode_batch_response(&method, &text, None),
            Ok(Response::Binary(data)) => {
                if SOURCE_FILE_RESPONSE_METHODS.contains(&method.as_str()) {
                    if data.is_empty() {
                        encode_batch_response(&method, "null", None)
                    } else {
                        let r =
                            format!("{{\"data\":{}}}", json::marshal_string(&base64_encode(&data)));
                        encode_batch_response(&method, &r, None)
                    }
                } else {
                    // Go marshals other RawBinary values as a JSON byte slice (base64 string).
                    encode_batch_response(
                        &method,
                        &json::marshal_string(&base64_encode(&data)),
                        None,
                    )
                }
            }
            Err(e) => encode_batch_response(&method, "null", Some(&e.to_string())),
        }
    }

    pub(crate) fn handle_batch_requests(&self, p: Params) -> ApiResult<String> {
        // Go `int`: zero or negative selects the default page size.
        let max = match p.get("maxResponseBytesPerPage") {
            Value::Number(n) if *n >= 1.0 => *n as usize,
            _ => DEFAULT_MAX_RESPONSE_BYTES_PER_PAGE,
        };
        let token = p.str("continuationToken")?;
        let encoded = if token.is_empty() {
            {
                // Raw params of every item, indexed in one pass (rescanning per item would be quadratic).
                let requests = p.array("requests")?;
                let mut raw = crate::predecode::current_batch_params().into_iter();
                requests
                    .iter()
                    .map(|r| self.handle_batch_request(r, raw.next().flatten()))
                    .collect()
            }
        } else {
            self.batch_pages
                .lock()
                .unwrap()
                .remove(token)
                .ok_or_else(|| ApiError::client("invalid batch continuation token"))?
        };
        Ok(self.paginate_batch_responses(encoded, max))
    }

    /// Go `paginateBatchResponses`.
    fn paginate_batch_responses(&self, encoded: Vec<String>, max: usize) -> String {
        let mut encoded_length = "{\"responses\":[]}".len();
        let mut page_length = 0;
        for e in &encoded {
            let additional = e.len() + usize::from(page_length > 0);
            if page_length > 0 && encoded_length + additional > max {
                break;
            }
            encoded_length += additional;
            page_length += 1;
        }
        let mut token = None;
        if page_length != encoded.len() {
            let t = format!("{}-{}", self.id(), self.next_batch_page_id());
            let continuation_length = ",\"continuationToken\":\"\"".len() + t.len();
            while page_length > 1 && encoded_length + continuation_length > max {
                encoded_length -= encoded[page_length - 1].len() + 1;
                page_length -= 1;
            }
            token = Some(t);
        }
        let mut encoded = encoded;
        let rest = encoded.split_off(page_length);
        let mut out = String::from("{\"responses\":[");
        out.push_str(&encoded.join(","));
        out.push(']');
        if let Some(t) = token {
            out.push_str(",\"continuationToken\":");
            out.push_str(&json::marshal_string(&t));
            self.batch_pages.lock().unwrap().insert(t, rest);
        }
        out.push('}');
        out
    }
}
