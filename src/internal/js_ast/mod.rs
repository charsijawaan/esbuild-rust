//! Port of upstream `internal/js_ast`.

mod unicode_data;

use unicode_data::{
    ID_CONTINUE_ES5_AND_ES_NEXT, ID_CONTINUE_ES5_OR_ES_NEXT, ID_START_ES5_AND_ES_NEXT,
    ID_START_ES5_OR_ES_NEXT, UnicodeRange,
};

#[must_use]
pub fn is_identifier(text: &str) -> bool {
    let mut characters = text.chars();
    characters.next().is_some_and(is_identifier_start) && characters.all(is_identifier_continue)
}

#[must_use]
pub fn is_identifier_es5_and_es_next(text: &str) -> bool {
    let mut characters = text.chars();
    characters
        .next()
        .is_some_and(is_identifier_start_es5_and_es_next)
        && characters.all(is_identifier_continue_es5_and_es_next)
}

#[must_use]
pub fn force_valid_identifier(prefix: &str, text: &str) -> String {
    let mut output = String::with_capacity(prefix.len() + text.len().max(1));
    output.push_str(prefix);
    let mut characters = text.chars();
    match characters.next() {
        Some(character) if is_identifier_start(character) => output.push(character),
        _ => output.push('_'),
    }
    for character in characters {
        output.push(if is_identifier_continue(character) {
            character
        } else {
            '_'
        });
    }
    output
}

#[must_use]
pub fn is_identifier_utf16(text: &[u16]) -> bool {
    is_identifier_utf16_with(
        text,
        is_identifier_start_code_point,
        is_identifier_continue_code_point,
    )
}

#[must_use]
pub fn is_identifier_es5_and_es_next_utf16(text: &[u16]) -> bool {
    is_identifier_utf16_with(
        text,
        is_identifier_start_es5_and_es_next_code_point,
        is_identifier_continue_es5_and_es_next_code_point,
    )
}

#[must_use]
pub fn is_identifier_start(character: char) -> bool {
    is_identifier_start_code_point(character as u32)
}

#[must_use]
pub fn is_identifier_continue(character: char) -> bool {
    is_identifier_continue_code_point(character as u32)
}

#[must_use]
pub fn is_identifier_start_es5_and_es_next(character: char) -> bool {
    is_identifier_start_es5_and_es_next_code_point(character as u32)
}

#[must_use]
pub fn is_identifier_continue_es5_and_es_next(character: char) -> bool {
    is_identifier_continue_es5_and_es_next_code_point(character as u32)
}

#[must_use]
pub const fn is_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200A}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

fn is_identifier_utf16_with(
    text: &[u16],
    is_start: fn(u32) -> bool,
    is_continue: fn(u32) -> bool,
) -> bool {
    if text.is_empty() {
        return false;
    }
    let mut index = 0;
    let mut first = true;
    while index < text.len() {
        let mut code_point = u32::from(text[index]);
        if (0xD800..=0xDBFF).contains(&code_point)
            && let Some(second @ 0xDC00..=0xDFFF) = text.get(index + 1).copied().map(u32::from)
        {
            code_point = 0x10000 + ((code_point - 0xD800) << 10) + (second - 0xDC00);
            index += 1;
        }
        if if first {
            !is_start(code_point)
        } else {
            !is_continue(code_point)
        } {
            return false;
        }
        first = false;
        index += 1;
    }
    true
}

fn is_identifier_start_code_point(code_point: u32) -> bool {
    is_ascii_identifier_start(code_point)
        || (code_point >= 0x7F && unicode_table_contains(ID_START_ES5_OR_ES_NEXT, code_point))
}

fn is_identifier_continue_code_point(code_point: u32) -> bool {
    is_ascii_identifier_continue(code_point)
        || matches!(code_point, 0x200C | 0x200D)
        || (code_point >= 0x7F && unicode_table_contains(ID_CONTINUE_ES5_OR_ES_NEXT, code_point))
}

fn is_identifier_start_es5_and_es_next_code_point(code_point: u32) -> bool {
    is_ascii_identifier_start(code_point)
        || (code_point >= 0x7F && unicode_table_contains(ID_START_ES5_AND_ES_NEXT, code_point))
}

fn is_identifier_continue_es5_and_es_next_code_point(code_point: u32) -> bool {
    is_ascii_identifier_continue(code_point)
        || matches!(code_point, 0x200C | 0x200D)
        || (code_point >= 0x7F && unicode_table_contains(ID_CONTINUE_ES5_AND_ES_NEXT, code_point))
}

const fn is_ascii_identifier_start(code_point: u32) -> bool {
    code_point == b'_' as u32
        || code_point == b'$' as u32
        || (code_point >= b'a' as u32 && code_point <= b'z' as u32)
        || (code_point >= b'A' as u32 && code_point <= b'Z' as u32)
}

const fn is_ascii_identifier_continue(code_point: u32) -> bool {
    is_ascii_identifier_start(code_point)
        || (code_point >= b'0' as u32 && code_point <= b'9' as u32)
}

fn unicode_table_contains(table: &[UnicodeRange], code_point: u32) -> bool {
    table.iter().any(|&(low, high, stride)| {
        code_point >= low && code_point <= high && (code_point - low).is_multiple_of(stride)
    })
}

#[cfg(test)]
mod tests {
    use super::{
        force_valid_identifier, is_identifier, is_identifier_es5_and_es_next,
        is_identifier_es5_and_es_next_utf16, is_identifier_utf16, is_whitespace,
    };

    #[test]
    fn validates_ascii_and_unicode_identifiers() {
        assert!(is_identifier("$hello_0"));
        assert!(!is_identifier(""));
        assert!(!is_identifier("0hello"));
        assert!(!is_identifier("hello-world"));
        assert!(is_identifier("π"));
        assert!(is_identifier("a\u{200c}b"));
        assert!(is_identifier_es5_and_es_next("π"));
        assert!(is_identifier("Ƞ"));
        assert!(!is_identifier_es5_and_es_next("Ƞ"));
    }

    #[test]
    fn validates_utf16_without_allocating() {
        assert!(is_identifier_utf16(&[u16::from(b'a'), u16::from(b'0')]));
        assert!(!is_identifier_utf16(&[u16::from(b'0'), u16::from(b'a')]));
        let astral = "𐊧x".encode_utf16().collect::<Vec<_>>();
        assert_eq!(is_identifier_utf16(&astral), is_identifier("𐊧x"));
        assert_eq!(
            is_identifier_es5_and_es_next_utf16(&astral),
            is_identifier_es5_and_es_next("𐊧x")
        );
        assert!(!is_identifier_utf16(&[0xD800]));
    }

    #[test]
    fn forces_valid_names_and_recognizes_ecmascript_whitespace() {
        assert_eq!(force_valid_identifier("", "0-a"), "__a");
        assert_eq!(force_valid_identifier("#", "field name"), "#field_name");
        assert_eq!(force_valid_identifier("", ""), "_");
        assert!(is_whitespace('\u{FEFF}'));
        assert!(!is_whitespace('\n'));
    }
}
