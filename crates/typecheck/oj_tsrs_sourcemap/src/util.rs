use tsrs_core::stringutil;

use super::*;

// util.go:11
// Tries to find the sourceMappingURL comment at the end of a file.
pub fn try_get_source_mapping_url(line_info: Option<&ECMALineInfo>) -> String {
    if let Some(line_info) = line_info {
        for index in (0..line_info.line_count()).rev() {
            let line = line_info.line_text(index);
            // Go unicode.IsSpace = the Unicode White_Space property = char::is_whitespace.
            let line = line.trim_start_matches(char::is_whitespace);
            let line = line.trim_end_matches(|c: char| stringutil::is_line_break(c as u32));
            if line.is_empty() {
                continue;
            }
            let bytes = line.as_bytes();
            if bytes.len() < 4
                || !line.starts_with("//")
                || bytes[2] != b'#' && bytes[2] != b'@'
                || bytes[3] != b' '
            {
                break;
            }
            if let Some(url) = line[4..].strip_prefix("sourceMappingURL=") {
                return url.trim_end_matches(char::is_whitespace).to_string();
            }
        }
    }
    String::new()
}
