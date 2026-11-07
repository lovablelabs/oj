// Standard (RFC 4648, padded) base64, matching Go's encoding/json marshaling of []byte.

use ::base64::Engine as _;

pub fn encode(data: &[u8]) -> String {
    ::base64::engine::general_purpose::STANDARD.encode(data)
}
