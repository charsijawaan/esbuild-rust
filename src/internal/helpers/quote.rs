// Port of upstream internal/helpers/quote.go.

use super::utf::decode_wtf8_rune;

const HEX_CHARS: &[u8; 16] = b"0123456789ABCDEF";
const FIRST_ASCII: u32 = 0x20;
const LAST_ASCII: u32 = 0x7e;
const FIRST_HIGH_SURROGATE: u32 = 0xd800;
const FIRST_LOW_SURROGATE: u32 = 0xdc00;
const LAST_LOW_SURROGATE: u32 = 0xdfff;

const fn can_print_without_escape(code_point: u32, ascii_only: bool) -> bool {
    if code_point <= LAST_ASCII {
        code_point >= FIRST_ASCII && code_point != 0x5c && code_point != 0x22
    } else {
        !ascii_only
            && code_point != 0xfeff
            && (code_point < FIRST_HIGH_SURROGATE || code_point > LAST_LOW_SURROGATE)
    }
}

#[must_use]
pub fn quote_single(text: &[u8], ascii_only: bool) -> Vec<u8> {
    internal_quote(text, ascii_only, b'\'')
}

#[must_use]
pub fn quote_for_json(text: &[u8], ascii_only: bool) -> Vec<u8> {
    internal_quote(text, ascii_only, b'"')
}

fn internal_quote(text: &[u8], ascii_only: bool, quote_char: u8) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + 2);
    let mut index = 0;
    bytes.push(quote_char);

    while index < text.len() {
        let (code_point, decoded_width) = decode_wtf8_rune(&text[index..]);
        let width = decoded_width.max(1);

        // Fast path: a run of characters that do not need escaping.
        if can_print_without_escape(code_point, ascii_only) {
            let start = index;
            index += width;
            while index < text.len() {
                let (next, decoded_width) = decode_wtf8_rune(&text[index..]);
                if !can_print_without_escape(next, ascii_only) {
                    break;
                }
                index += decoded_width.max(1);
            }
            bytes.extend_from_slice(&text[start..index]);
            continue;
        }

        match code_point {
            0x08 => bytes.extend_from_slice(br"\b"),
            0x0c => bytes.extend_from_slice(br"\f"),
            0x0a => bytes.extend_from_slice(br"\n"),
            0x0d => bytes.extend_from_slice(br"\r"),
            0x09 => bytes.extend_from_slice(br"\t"),
            0x5c => bytes.extend_from_slice(br"\\"),
            0x22 if quote_char == b'"' => bytes.extend_from_slice(br#"\""#),
            0x22 => bytes.push(b'"'),
            0x27 if quote_char == b'\'' => bytes.extend_from_slice(br"\'"),
            0x27 => bytes.push(b'\''),
            mut other => {
                if other <= 0xffff {
                    append_unicode_escape(&mut bytes, other);
                } else {
                    other -= 0x1_0000;
                    let high = FIRST_HIGH_SURROGATE + ((other >> 10) & 0x3ff);
                    let low = FIRST_LOW_SURROGATE + (other & 0x3ff);
                    append_unicode_escape(&mut bytes, high);
                    append_unicode_escape(&mut bytes, low);
                }
            }
        }
        index += width;
    }

    bytes.push(quote_char);
    bytes
}

fn append_unicode_escape(bytes: &mut Vec<u8>, code_point: u32) {
    bytes.extend_from_slice(&[
        b'\\',
        b'u',
        HEX_CHARS[((code_point >> 12) & 15) as usize],
        HEX_CHARS[((code_point >> 8) & 15) as usize],
        HEX_CHARS[((code_point >> 4) & 15) as usize],
        HEX_CHARS[(code_point & 15) as usize],
    ]);
}

#[cfg(test)]
mod tests {
    use super::{quote_for_json, quote_single};

    #[test]
    fn quotes_json_and_javascript_strings() {
        assert_eq!(quote_for_json(b"one\n\"two\"", false), br#""one\n\"two\"""#);
        // This intentionally mirrors upstream's fast path, which currently
        // treats a single quote as directly printable.
        assert_eq!(quote_single(b"one'two", false), br"'one'two'");
        assert_eq!(quote_single(br#"one"two"#, false), br#"'one"two'"#);
    }

    #[test]
    fn handles_ascii_only_non_bmp_and_wtf8_surrogates() {
        assert_eq!(quote_for_json("🙂".as_bytes(), true), br#""\uD83D\uDE42""#);
        assert_eq!(
            quote_for_json("🙂".as_bytes(), false),
            ["\"".as_bytes(), "🙂".as_bytes(), "\"".as_bytes()].concat()
        );
        assert_eq!(quote_for_json(&[0xed, 0xa0, 0x80], false), br#""\uD800""#);
        assert_eq!(quote_for_json("\u{feff}".as_bytes(), false), br#""\uFEFF""#);
    }
}
