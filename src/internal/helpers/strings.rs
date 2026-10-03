// Port of upstream internal/helpers/strings.go.

#[must_use]
pub fn string_arrays_equal(a: &[String], b: &[String]) -> bool {
    a == b
}

#[must_use]
pub fn string_array_arrays_equal(a: &[Vec<String>], b: &[Vec<String>]) -> bool {
    a == b
}

#[must_use]
pub fn string_array_to_quoted_comma_separated_string(a: &[String]) -> String {
    let mut result = String::new();
    for (index, text) in a.iter().enumerate() {
        if index > 0 {
            result.push_str(", ");
        }
        write_go_quoted_string(&mut result, text.as_bytes());
    }
    result
}

/// Formats a Go `%q` string, including non-printable Unicode and invalid UTF-8.
/// Namespace string keys can contain WTF-8 surrogate bytes, so accept bytes.
#[must_use]
pub fn quote_go_string(text: &[u8]) -> String {
    let mut result = String::new();
    write_go_quoted_string(&mut result, text);
    result
}

fn write_go_quoted_string(result: &mut String, text: &[u8]) {
    use std::{fmt::Write, sync::OnceLock};

    // Go 1.26.5, used for the pinned reference binary, has Unicode 15 tables.
    // IsPrint accepts L, M, N, P, S, and ASCII space. Intersect with Age to
    // avoid printing newly assigned characters from regex's newer Unicode tables.
    static PRINTABLE: OnceLock<regex::Regex> = OnceLock::new();
    let printable = PRINTABLE.get_or_init(|| {
        regex::Regex::new(r"\A[\p{Age=15.0}&&[\p{L}\p{M}\p{N}\p{P}\p{S}]]\z")
            .expect("valid Unicode category expression")
    });
    result.push('"');
    let mut index = 0;
    while index < text.len() {
        let byte = text[index];
        let width = if byte < 0x80 {
            1
        } else if byte < 0xe0 {
            2
        } else if byte < 0xf0 {
            3
        } else {
            4
        };
        let Some(encoded) = text
            .get(index..index + width)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
        else {
            write!(result, "\\x{byte:02x}").expect("writing to a string cannot fail");
            index += 1;
            continue;
        };
        let c = encoded.chars().next().expect("non-empty encoded character");
        match c {
            '\u{0007}' => result.push_str("\\a"),
            '\u{0008}' => result.push_str("\\b"),
            '\u{000C}' => result.push_str("\\f"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\u{000B}' => result.push_str("\\v"),
            '\\' => result.push_str("\\\\"),
            '"' => result.push_str("\\\""),
            '\u{0000}'..='\u{001F}' | '\u{007F}' => {
                write!(result, "\\x{:02x}", u32::from(c)).expect("writing to a string cannot fail");
            }
            _ if c.is_ascii() || printable.is_match(encoded) => result.push(c),
            _ if u32::from(c) <= 0xffff => {
                write!(result, "\\u{:04x}", u32::from(c)).expect("writing to a string cannot fail");
            }
            _ => {
                write!(result, "\\U{:08x}", u32::from(c)).expect("writing to a string cannot fail");
            }
        }
        index += width;
    }
    result.push('"');
}

#[cfg(test)]
mod tests {
    use super::{
        quote_go_string, string_array_arrays_equal, string_array_to_quoted_comma_separated_string,
        string_arrays_equal,
    };

    #[test]
    fn equality_helpers_match_slice_equality() {
        let a = vec!["a".to_string(), "b".to_string()];
        let b = a.clone();
        assert!(string_arrays_equal(&a, &b));
        assert!(string_array_arrays_equal(
            std::slice::from_ref(&a),
            std::slice::from_ref(&b)
        ));
        assert!(!string_arrays_equal(&a, &["a".to_string()]));
    }

    #[test]
    fn comma_separated_strings_use_go_style_quoting() {
        assert_eq!(
            string_array_to_quoted_comma_separated_string(&[
                "a".to_string(),
                "b\n\"c".to_string(),
                "\u{0007}".to_string(),
            ]),
            "\"a\", \"b\\n\\\"c\", \"\\a\""
        );
    }

    #[test]
    fn quotes_go_unicode_categories_controls_and_invalid_bytes() {
        for (input, expected) in [
            ("q\n\u{1}".as_bytes(), r#""q\n\x01""#),
            ("e\u{301}".as_bytes(), "\"e\u{301}\""),
            ("\u{feff}".as_bytes(), r#""\ufeff""#),
            ("a\u{200c}\u{200d}".as_bytes(), r#""a\u200c\u200d""#),
            (
                "\u{a0}\u{e000}\u{e0001}".as_bytes(),
                r#""\u00a0\ue000\U000e0001""#,
            ),
            ("π🙂".as_bytes(), "\"π🙂\""),
            ("\u{1c89}\u{1fae9}".as_bytes(), r#""\u1c89\U0001fae9""#),
            (&[1, 7, 11, 127][..], r#""\x01\a\v\x7f""#),
            (&[0xed, 0xa0, 0x80, 0xff][..], r#""\xed\xa0\x80\xff""#),
        ] {
            assert_eq!(quote_go_string(input), expected);
        }
    }
}
