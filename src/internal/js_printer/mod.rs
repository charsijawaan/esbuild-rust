//! Port of upstream `internal/js_printer`.

use crate::internal::compat::JsFeature;

const HEX_CHARS: &[u8; 16] = b"0123456789ABCDEF";
const FIRST_ASCII: u32 = 0x20;
const LAST_ASCII: u32 = 0x7e;
const FIRST_HIGH_SURROGATE: u16 = 0xd800;
const LAST_HIGH_SURROGATE: u16 = 0xdbff;
const FIRST_LOW_SURROGATE: u16 = 0xdc00;
const LAST_LOW_SURROGATE: u16 = 0xdfff;

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub unsupported_features: JsFeature,
    pub line_limit: usize,
    pub minify_syntax: bool,
    pub ascii_only: bool,
}

#[must_use]
/// Appends an identifier, escaping non-ASCII code points for ASCII-only output.
///
/// # Panics
///
/// Panics when an astral code point cannot be represented because Unicode code
/// point escapes are unsupported by the configured target.
pub fn quote_identifier(
    mut output: Vec<u8>,
    name: &str,
    unsupported_features: JsFeature,
) -> Vec<u8> {
    let mut ascii_start = 0;
    let mut is_ascii = false;
    for (index, character) in name.char_indices() {
        let code_point = character as u32;
        if (FIRST_ASCII..=LAST_ASCII).contains(&code_point) {
            if !is_ascii {
                is_ascii = true;
                ascii_start = index;
            }
            continue;
        }
        if is_ascii {
            output.extend_from_slice(&name.as_bytes()[ascii_start..index]);
            is_ascii = false;
        }
        if let Ok(code_unit) = u16::try_from(code_point) {
            push_u16_escape(&mut output, code_unit);
        } else if !unsupported_features.contains(JsFeature::UNICODE_ESCAPES) {
            output.extend_from_slice(format!("\\u{{{code_point:X}}}").as_bytes());
        } else {
            panic!("Internal error: Cannot encode identifier: Unicode escapes are unsupported");
        }
    }
    if is_ascii {
        output.extend_from_slice(&name.as_bytes()[ascii_start..]);
    }
    output
}

#[must_use]
pub fn quote_utf16(data: &[u16], options: Options, allow_backtick: bool) -> Vec<u8> {
    let allow_backtick = allow_backtick
        && !options
            .unsupported_features
            .contains(JsFeature::TEMPLATE_LITERAL);
    let mut single_cost = 0;
    let mut double_cost = 0;
    let mut backtick_cost = 0;
    for (index, code_unit) in data.iter().copied().enumerate() {
        match code_unit {
            10 if options.minify_syntax => backtick_cost -= 1,
            39 => single_cost += 1,
            34 => double_cost += 1,
            96 => backtick_cost += 1,
            36 if data.get(index + 1) == Some(&u16::from(b'{')) => backtick_cost += 1,
            _ => {}
        }
    }

    let quote = if double_cost > single_cost {
        if allow_backtick && single_cost > backtick_cost {
            b'`'
        } else {
            b'\''
        }
    } else if allow_backtick && double_cost > backtick_cost {
        b'`'
    } else {
        b'"'
    };
    let mut output = vec![quote];
    print_unquoted_utf16(&mut output, data, quote, options);
    output.push(quote);
    output
}

#[allow(clippy::too_many_lines)]
fn print_unquoted_utf16(output: &mut Vec<u8>, text: &[u16], quote: u8, options: Options) {
    let mut index = 0;
    let mut start_line_length = output
        .iter()
        .rev()
        .position(|byte| *byte == b'\n')
        .map_or(output.len(), |position| position);
    if options.line_limit > 0 {
        start_line_length = start_line_length.min(options.line_limit);
    }

    while index < text.len() {
        if options.line_limit > 0 && start_line_length + index >= options.line_limit {
            output.extend_from_slice(b"\\\n");
            start_line_length = start_line_length.saturating_sub(options.line_limit);
        }

        let code_unit = text[index];
        index += 1;
        match code_unit {
            0 => {
                if text
                    .get(index)
                    .is_some_and(|next| (u16::from(b'0')..=u16::from(b'9')).contains(next))
                {
                    output.extend_from_slice(b"\\x00");
                } else {
                    output.extend_from_slice(b"\\0");
                }
            }
            7 => output.extend_from_slice(b"\\x07"),
            8 => output.extend_from_slice(b"\\b"),
            12 => output.extend_from_slice(b"\\f"),
            10 if quote == b'`' => {
                start_line_length = 0;
                output.push(b'\n');
            }
            10 => output.extend_from_slice(b"\\n"),
            13 => output.extend_from_slice(b"\\r"),
            11 => output.extend_from_slice(b"\\v"),
            0x1b => output.extend_from_slice(b"\\x1B"),
            92 => output.extend_from_slice(b"\\\\"),
            47 => {
                if !options
                    .unsupported_features
                    .contains(JsFeature::INLINE_SCRIPT)
                    && index >= 2
                    && text[index - 2] == u16::from(b'<')
                    && index + 6 <= text.len()
                    && text[index..index + 6]
                        .iter()
                        .copied()
                        .map(|unit| u8::try_from(unit).unwrap_or_default().to_ascii_lowercase())
                        .eq(b"script".iter().copied())
                {
                    output.push(b'\\');
                }
                output.push(b'/');
            }
            39 if quote == b'\'' => output.extend_from_slice(b"\\'"),
            34 if quote == b'"' => output.extend_from_slice(b"\\\""),
            96 if quote == b'`' => output.extend_from_slice(b"\\`"),
            36 if quote == b'`' && text.get(index) == Some(&u16::from(b'{')) => {
                output.extend_from_slice(b"\\$");
            }
            0x2028 => output.extend_from_slice(b"\\u2028"),
            0x2029 => output.extend_from_slice(b"\\u2029"),
            0xfeff => output.extend_from_slice(b"\\uFEFF"),
            code_unit if u32::from(code_unit) <= LAST_ASCII => {
                output.push(u8::try_from(code_unit).expect("ASCII code unit"));
            }
            high if (FIRST_HIGH_SURROGATE..=LAST_HIGH_SURROGATE).contains(&high) => {
                if let Some(&low) = text.get(index)
                    && (FIRST_LOW_SURROGATE..=LAST_LOW_SURROGATE).contains(&low)
                {
                    index += 1;
                    let code_point = ((u32::from(high) - u32::from(FIRST_HIGH_SURROGATE)) << 10)
                        + (u32::from(low) - u32::from(FIRST_LOW_SURROGATE))
                        + 0x1_0000;
                    if options.ascii_only {
                        if options
                            .unsupported_features
                            .contains(JsFeature::UNICODE_ESCAPES)
                        {
                            push_u16_escape(output, high);
                            push_u16_escape(output, low);
                        } else {
                            output.extend_from_slice(format!("\\u{{{code_point:X}}}").as_bytes());
                        }
                    } else if let Some(character) = char::from_u32(code_point) {
                        let mut bytes = [0; 4];
                        output.extend_from_slice(character.encode_utf8(&mut bytes).as_bytes());
                    }
                    continue;
                }
                push_u16_escape(output, high);
            }
            code_unit
                if (FIRST_LOW_SURROGATE..=LAST_LOW_SURROGATE).contains(&code_unit)
                    || (options.ascii_only && code_unit > u16::from(u8::MAX)) =>
            {
                push_u16_escape(output, code_unit);
            }
            code_unit if options.ascii_only => {
                output.extend_from_slice(b"\\x");
                output.push(HEX_CHARS[usize::from(code_unit >> 4)]);
                output.push(HEX_CHARS[usize::from(code_unit & 15)]);
            }
            code_unit => {
                if let Some(character) = char::from_u32(u32::from(code_unit)) {
                    let mut bytes = [0; 4];
                    output.extend_from_slice(character.encode_utf8(&mut bytes).as_bytes());
                }
            }
        }
    }
}

fn push_u16_escape(output: &mut Vec<u8>, code_unit: u16) {
    output.extend_from_slice(b"\\u");
    output.push(HEX_CHARS[usize::from(code_unit >> 12)]);
    output.push(HEX_CHARS[usize::from((code_unit >> 8) & 15)]);
    output.push(HEX_CHARS[usize::from((code_unit >> 4) & 15)]);
    output.push(HEX_CHARS[usize::from(code_unit & 15)]);
}

#[cfg(test)]
mod tests {
    use super::{Options, quote_identifier, quote_utf16};
    use crate::internal::{compat::JsFeature, helpers::string_to_utf16};

    fn quoted(text: &str, options: Options, allow_backtick: bool) -> String {
        String::from_utf8(quote_utf16(
            &string_to_utf16(text.as_bytes()),
            options,
            allow_backtick,
        ))
        .expect("printer output is UTF-8")
    }

    #[test]
    fn quotes_identifiers_for_ascii_output() {
        assert_eq!(
            String::from_utf8(quote_identifier(Vec::new(), "π_value", JsFeature::NONE)).unwrap(),
            "\\u03C0_value"
        );
        assert_eq!(
            String::from_utf8(quote_identifier(Vec::new(), "𐊧x", JsFeature::NONE)).unwrap(),
            "\\u{102A7}x"
        );
    }

    #[test]
    fn chooses_the_shortest_string_delimiter() {
        assert_eq!(quoted("a\"b", Options::default(), false), "'a\"b'");
        assert_eq!(quoted("a\"'b", Options::default(), true), "`a\"'b`");
        assert_eq!(quoted("${x}", Options::default(), true), "\"${x}\"");
    }

    #[test]
    fn escapes_javascript_string_hazards() {
        assert_eq!(quoted("\0x", Options::default(), false), "\"\\0x\"");
        assert_eq!(quoted("\x001", Options::default(), false), "\"\\x001\"");
        assert_eq!(
            quoted("</ScRiPt>", Options::default(), false),
            "\"<\\/ScRiPt>\""
        );
        assert_eq!(
            quoted("\u{2028}\u{2029}\u{feff}", Options::default(), false),
            "\"\\u2028\\u2029\\uFEFF\""
        );
    }

    #[test]
    fn prints_utf16_as_utf8_or_ascii_escapes() {
        assert_eq!(quoted("π😀", Options::default(), false), "\"π😀\"");
        assert_eq!(
            quoted(
                "π😀",
                Options {
                    ascii_only: true,
                    ..Options::default()
                },
                false
            ),
            "\"\\u03C0\\u{1F600}\""
        );
        assert_eq!(
            quoted(
                "😀",
                Options {
                    unsupported_features: JsFeature::UNICODE_ESCAPES,
                    ascii_only: true,
                    ..Options::default()
                },
                false
            ),
            "\"\\uD83D\\uDE00\""
        );
    }
}
