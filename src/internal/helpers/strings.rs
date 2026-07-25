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
        write_go_quoted_string(&mut result, text);
    }
    result
}

fn write_go_quoted_string(result: &mut String, text: &str) {
    result.push('"');
    for c in text.chars() {
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
                use std::fmt::Write;
                write!(result, "\\x{:02x}", u32::from(c)).expect("writing to a string cannot fail");
            }
            _ => result.push(c),
        }
    }
    result.push('"');
}

#[cfg(test)]
mod tests {
    use super::{
        string_array_arrays_equal, string_array_to_quoted_comma_separated_string,
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
}
