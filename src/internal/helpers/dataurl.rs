// Port of upstream internal/helpers/dataurl.go.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// Returns the shorter of either a base64-encoded or percent-escaped data URL.
#[must_use]
pub fn encode_string_as_shortest_data_url(mime_type: &str, text: &[u8]) -> String {
    let encoded = STANDARD.encode(text);
    let url = format!("data:{mime_type};base64,{encoded}");
    if let Some(percent_url) = encode_string_as_percent_escaped_data_url(mime_type, text)
        && percent_url.len() < url.len()
    {
        return percent_url;
    }
    url
}

/// Returns `None` when `text` contains invalid UTF-8.
#[must_use]
pub fn encode_string_as_percent_escaped_data_url(mime_type: &str, text: &[u8]) -> Option<String> {
    std::str::from_utf8(text).ok()?;

    let hex = b"0123456789ABCDEF";
    let mut result = format!("data:{mime_type},");
    let mut run_start = 0;

    // Scan for trailing characters that need to be escaped.
    let mut trailing_start = text.len();
    while trailing_start > 0 {
        let c = text[trailing_start - 1];
        if c > 0x20 || matches!(c, b'\t' | b'\n' | b'\r') {
            break;
        }
        trailing_start -= 1;
    }

    for (index, c) in text.iter().copied().enumerate() {
        let escape = matches!(c, b'\t' | b'\n' | b'\r' | b'#')
            || index >= trailing_start
            || (c == b'%'
                && index + 2 < text.len()
                && is_hex(text[index + 1])
                && is_hex(text[index + 2]));
        if escape {
            if run_start < index {
                append_utf8(&mut result, &text[run_start..index]);
            }
            result.push('%');
            result.push(char::from(hex[usize::from(c >> 4)]));
            result.push(char::from(hex[usize::from(c & 15)]));
            run_start = index + 1;
        }
    }

    if run_start < text.len() {
        append_utf8(&mut result, &text[run_start..]);
    }
    Some(result)
}

fn append_utf8(result: &mut String, bytes: &[u8]) {
    result.push_str(std::str::from_utf8(bytes).expect("input was validated as UTF-8"));
}

const fn is_hex(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

#[cfg(test)]
mod tests {
    use super::{encode_string_as_percent_escaped_data_url, encode_string_as_shortest_data_url};

    fn check(raw: &str, expected: &str) {
        assert_eq!(
            encode_string_as_percent_escaped_data_url("text/plain", raw.as_bytes()).as_deref(),
            Some(expected),
            "failed for {raw:?}"
        );
    }

    #[test]
    fn encode_data_url_matches_upstream_exhaustive_test() {
        for value in 0..=0xff {
            let character = char::from_u32(value).expect("test value is a character");
            let raw = character.to_string();
            let always_escape = matches!(character, '\t' | '\r' | '\n' | '#');
            let trailing_escape = value <= 0x20 || character == '#';

            if trailing_escape {
                check(&raw, &format!("data:text/plain,%{value:02X}"));
                check(
                    &format!("foo{raw}"),
                    &format!("data:text/plain,foo%{value:02X}"),
                );
            } else {
                check(&raw, &format!("data:text/plain,{character}"));
                check(
                    &format!("foo{raw}"),
                    &format!("data:text/plain,foo{character}"),
                );
            }

            if always_escape {
                check(
                    &format!("{raw}foo"),
                    &format!("data:text/plain,%{value:02X}foo"),
                );
            } else {
                check(
                    &format!("{raw}foo"),
                    &format!("data:text/plain,{character}foo"),
                );
            }
        }

        check(" \t ", "data:text/plain, %09%20");
        check(" \n ", "data:text/plain, %0A%20");
        check(" \r ", "data:text/plain, %0D%20");
        check(" # ", "data:text/plain, %23%20");
        check("\u{0008}#\u{0008}", "data:text/plain,\u{0008}%23%08");
        check("%, %3, %33, %333", "data:text/plain,%, %3, %2533, %25333");
    }

    #[test]
    fn invalid_utf8_falls_back_to_base64() {
        let invalid = [0xff];
        assert_eq!(
            encode_string_as_percent_escaped_data_url("x", &invalid),
            None
        );
        assert_eq!(
            encode_string_as_shortest_data_url("x", &invalid),
            "data:x;base64,/w=="
        );
    }
}
