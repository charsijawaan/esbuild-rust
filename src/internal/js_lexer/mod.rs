//! Partial port of `internal/js_lexer`.
//!
//! The reserved-word tables are used by the renamer. Tokenization will be
//! added with the rest of this package.

pub const KEYWORDS: &[&str] = &[
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "new",
    "null",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
];

pub const STRICT_MODE_RESERVED_WORDS: &[&str] = &[
    "implements",
    "interface",
    "let",
    "package",
    "private",
    "protected",
    "public",
    "static",
    "yield",
];

#[must_use]
pub fn is_keyword(name: &str) -> bool {
    KEYWORDS.contains(&name)
}

#[must_use]
pub fn is_strict_mode_reserved_word(name: &str) -> bool {
    STRICT_MODE_RESERVED_WORDS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::{KEYWORDS, STRICT_MODE_RESERVED_WORDS, is_keyword, is_strict_mode_reserved_word};

    #[test]
    fn keyword_tables_match_upstream_sizes_and_boundaries() {
        assert_eq!(KEYWORDS.len(), 36);
        assert_eq!(STRICT_MODE_RESERVED_WORDS.len(), 9);
        assert!(is_keyword("break"));
        assert!(is_keyword("with"));
        assert!(!is_keyword("await"));
        assert!(is_strict_mode_reserved_word("implements"));
        assert!(is_strict_mode_reserved_word("yield"));
        assert!(!is_strict_mode_reserved_word("async"));
    }
}
