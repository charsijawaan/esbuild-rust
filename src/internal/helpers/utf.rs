// Port of upstream internal/helpers/utf.go.

pub const REPLACEMENT_CHARACTER: u32 = 0xfffd;
const MAX_RUNE: u32 = 0x10_ffff;

#[must_use]
pub fn contains_non_bmp_code_point(text: &[u8]) -> bool {
    let mut index = 0;
    while index < text.len() {
        let (code_point, width) = decode_wtf8_rune(&text[index..]);
        if code_point > 0xffff {
            return true;
        }
        index += width.max(1);
    }
    false
}

/// Does `contains_non_bmp_code_point(utf16_to_string(text))` without allocating.
#[must_use]
pub fn contains_non_bmp_code_point_utf16(text: &[u16]) -> bool {
    text.windows(2)
        .any(|pair| (0xd800..=0xdbff).contains(&pair[0]) && (0xdc00..=0xdfff).contains(&pair[1]))
}

#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn string_to_utf16(text: &[u8]) -> Vec<u16> {
    let mut decoded = Vec::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        let (mut code_point, width) = decode_wtf8_rune(&text[index..]);
        index += width.max(1);
        if code_point <= 0xffff {
            decoded.push(code_point as u16);
        } else {
            code_point -= 0x1_0000;
            decoded.push(0xd800 + ((code_point >> 10) & 0x3ff) as u16);
            decoded.push(0xdc00 + (code_point & 0x3ff) as u16);
        }
    }
    decoded
}

/// Converts UTF-16 to potentially non-UTF-8 WTF-8 bytes.
#[must_use]
pub fn utf16_to_string(text: &[u16]) -> Vec<u8> {
    let mut result = Vec::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        let mut code_point = u32::from(text[index]);
        if (0xd800..=0xdbff).contains(&code_point) && index + 1 < text.len() {
            let second = u32::from(text[index + 1]);
            if (0xdc00..=0xdfff).contains(&second) {
                code_point = (((code_point - 0xd800) << 10) | (second - 0xdc00)) + 0x1_0000;
                index += 1;
            }
        }
        encode_wtf8_rune(&mut result, code_point);
        index += 1;
    }
    result
}

/// Converts valid UTF-16 to a Rust string or returns the first unpaired
/// surrogate.
///
/// # Errors
///
/// Returns the first unpaired surrogate code unit.
///
/// # Panics
///
/// Panics only if the private WTF-8 encoder produces invalid UTF-8 for input
/// that has already passed surrogate validation.
pub fn utf16_to_string_with_validation(text: &[u16]) -> Result<String, u16> {
    let mut result = Vec::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        let first = text[index];
        let mut code_point = u32::from(first);
        if (0xd800..=0xdbff).contains(&first) {
            let Some(&second) = text.get(index + 1) else {
                return Err(first);
            };
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err(first);
            }
            code_point = (((code_point - 0xd800) << 10) | (u32::from(second) - 0xdc00)) + 0x1_0000;
            index += 1;
        } else if (0xdc00..=0xdfff).contains(&first) {
            return Err(first);
        }
        encode_wtf8_rune(&mut result, code_point);
        index += 1;
    }

    Ok(String::from_utf8(result).expect("validated UTF-16 always produces UTF-8"))
}

/// Does `utf16_to_string(text) == string` without a temporary allocation.
#[must_use]
pub fn utf16_equals_wtf8(text: &[u16], string: &[u8]) -> bool {
    if text.len() > string.len() {
        // UTF-16 encoding cannot be longer than equal WTF-8 encoding.
        return false;
    }

    let mut index = 0;
    let mut string_index = 0;
    while index < text.len() {
        let mut code_point = u32::from(text[index]);
        if (0xd800..=0xdbff).contains(&code_point) && index + 1 < text.len() {
            let second = u32::from(text[index + 1]);
            if (0xdc00..=0xdfff).contains(&second) {
                code_point = (((code_point - 0xd800) << 10) | (second - 0xdc00)) + 0x1_0000;
                index += 1;
            }
        }

        let mut encoded = Vec::with_capacity(4);
        encode_wtf8_rune(&mut encoded, code_point);
        let Some(candidate) = string.get(string_index..string_index + encoded.len()) else {
            return false;
        };
        if candidate != encoded {
            return false;
        }
        string_index += encoded.len();
        index += 1;
    }
    string_index == string.len()
}

#[must_use]
pub fn utf16_equals_utf16(a: &[u16], b: &[u16]) -> bool {
    a == b
}

/// Decodes one WTF-8 code point.
///
/// The returned width is zero for an empty or truncated multi-byte sequence,
/// matching esbuild's modified clone of Go's `utf8.DecodeRuneInString`.
#[must_use]
pub fn decode_wtf8_rune(string: &[u8]) -> (u32, usize) {
    let Some(&first) = string.first() else {
        return (REPLACEMENT_CHARACTER, 0);
    };
    if first < 0x80 {
        return (u32::from(first), 1);
    }

    let size = if first & 0xe0 == 0xc0 {
        2
    } else if first & 0xf0 == 0xe0 {
        3
    } else if first & 0xf8 == 0xf0 {
        4
    } else {
        return (REPLACEMENT_CHARACTER, 1);
    };
    if string.len() < size {
        return (REPLACEMENT_CHARACTER, 0);
    }

    let second = string[1];
    if second & 0xc0 != 0x80 {
        return (REPLACEMENT_CHARACTER, 1);
    }
    if size == 2 {
        let code_point = u32::from(first & 0x1f) << 6 | u32::from(second & 0x3f);
        if code_point < 0x80 {
            return (REPLACEMENT_CHARACTER, 1);
        }
        return (code_point, 2);
    }

    let third = string[2];
    if third & 0xc0 != 0x80 {
        return (REPLACEMENT_CHARACTER, 1);
    }
    if size == 3 {
        let code_point =
            u32::from(first & 0x0f) << 12 | u32::from(second & 0x3f) << 6 | u32::from(third & 0x3f);
        if code_point < 0x800 {
            return (REPLACEMENT_CHARACTER, 1);
        }
        return (code_point, 3);
    }

    let fourth = string[3];
    if fourth & 0xc0 != 0x80 {
        return (REPLACEMENT_CHARACTER, 1);
    }
    let code_point = u32::from(first & 0x07) << 18
        | u32::from(second & 0x3f) << 12
        | u32::from(third & 0x3f) << 6
        | u32::from(fourth & 0x3f);
    if !(0x1_0000..=MAX_RUNE).contains(&code_point) {
        return (REPLACEMENT_CHARACTER, 1);
    }
    (code_point, 4)
}

#[allow(clippy::cast_possible_truncation)]
fn encode_wtf8_rune(result: &mut Vec<u8>, mut code_point: u32) {
    if code_point <= 0x7f {
        result.push(code_point as u8);
    } else if code_point <= 0x7ff {
        result.push(0xc0 | (code_point >> 6) as u8);
        result.push(0x80 | (code_point & 0x3f) as u8);
    } else {
        if code_point > MAX_RUNE {
            code_point = REPLACEMENT_CHARACTER;
        }
        if code_point <= 0xffff {
            result.push(0xe0 | (code_point >> 12) as u8);
            result.push(0x80 | ((code_point >> 6) & 0x3f) as u8);
            result.push(0x80 | (code_point & 0x3f) as u8);
        } else {
            result.push(0xf0 | (code_point >> 18) as u8);
            result.push(0x80 | ((code_point >> 12) & 0x3f) as u8);
            result.push(0x80 | ((code_point >> 6) & 0x3f) as u8);
            result.push(0x80 | (code_point & 0x3f) as u8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        REPLACEMENT_CHARACTER, contains_non_bmp_code_point, contains_non_bmp_code_point_utf16,
        decode_wtf8_rune, string_to_utf16, utf16_equals_wtf8, utf16_to_string,
        utf16_to_string_with_validation,
    };

    #[test]
    fn preserves_unpaired_surrogates_as_wtf8() {
        let utf16 = [0xd800, b'a'.into(), 0xdfff];
        let wtf8 = utf16_to_string(&utf16);
        assert_eq!(wtf8, [0xed, 0xa0, 0x80, b'a', 0xed, 0xbf, 0xbf]);
        assert_eq!(string_to_utf16(&wtf8), utf16);
        assert!(utf16_equals_wtf8(&utf16, &wtf8));
        assert_eq!(utf16_to_string_with_validation(&utf16), Err(0xd800));
    }

    #[test]
    fn combines_surrogate_pairs() {
        let utf16 = [0xd83d, 0xde42];
        let utf8 = "🙂".as_bytes();
        assert_eq!(utf16_to_string(&utf16), utf8);
        assert_eq!(utf16_to_string_with_validation(&utf16).as_deref(), Ok("🙂"));
        assert!(contains_non_bmp_code_point(utf8));
        assert!(contains_non_bmp_code_point_utf16(&utf16));
        assert!(utf16_equals_wtf8(&utf16, utf8));
    }

    #[test]
    fn round_trips_every_single_utf16_code_unit() {
        for code_unit in 0..=u16::MAX {
            let utf16 = [code_unit];
            assert_eq!(string_to_utf16(&utf16_to_string(&utf16)), utf16);
        }
    }

    #[test]
    fn rejects_invalid_and_truncated_sequences_like_upstream() {
        assert_eq!(decode_wtf8_rune(&[]), (REPLACEMENT_CHARACTER, 0));
        assert_eq!(decode_wtf8_rune(&[0xf0]), (REPLACEMENT_CHARACTER, 0));
        assert_eq!(decode_wtf8_rune(&[0xc0, 0x80]), (REPLACEMENT_CHARACTER, 1));
        assert_eq!(
            decode_wtf8_rune(&[0xf4, 0x90, 0x80, 0x80]),
            (REPLACEMENT_CHARACTER, 1)
        );
    }
}
