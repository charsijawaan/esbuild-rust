//! Port of `internal/js_lexer`.

mod lexer;
mod tables;

pub use lexer::{
    CommentBefore, JsonFlavor, KeyOrValue, Lexer, LexerPanic, MaybeSubstring, range_of_identifier,
    range_of_import_assert_or_with,
};
pub use tables::{
    JSX_ENTITY_COUNT, T, TOKEN_COUNT, Token, jsx_entity, keyword_token, token_to_string,
};

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
    keyword_token(name).is_some()
}

#[must_use]
pub fn is_strict_mode_reserved_word(name: &str) -> bool {
    STRICT_MODE_RESERVED_WORDS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::{
        JSX_ENTITY_COUNT, KEYWORDS, STRICT_MODE_RESERVED_WORDS, TOKEN_COUNT, Token, is_keyword,
        is_strict_mode_reserved_word, jsx_entity, keyword_token, token_to_string,
    };

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

    #[test]
    fn generated_token_tables_match_upstream() {
        assert_eq!(TOKEN_COUNT, 107);
        assert_eq!(JSX_ENTITY_COUNT, 253);
        assert_eq!(keyword_token("break"), Some(Token::Break));
        assert_eq!(keyword_token("with"), Some(Token::With));
        assert_eq!(keyword_token("await"), None);
        assert_eq!(token_to_string(Token::EqualsGreaterThan), "\"=>\"");
        assert_eq!(token_to_string(Token::Identifier), "identifier");
        assert!(Token::Equals.is_assign());
        assert!(Token::SlashEquals.is_assign());
        assert!(!Token::Slash.is_assign());
        assert_eq!(jsx_entity("amp"), Some('&'));
        assert_eq!(jsx_entity("Omega"), Some('\u{03A9}'));
        assert_eq!(jsx_entity("missing"), None);
    }
}
