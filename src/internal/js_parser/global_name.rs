use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::internal::{
    helpers::utf16_to_string,
    js_lexer::{Lexer, LexerPanic, Token},
    logger::{Log, Source},
};

/// Parse a dotted/indexed JavaScript global name.
///
/// The returned path components remain WTF-8 bytes, matching Go strings and
/// preserving lone surrogates without forcing them into a Rust `String`.
#[must_use]
pub fn parse_global_name(log: Log, source: Source) -> (Vec<Vec<u8>>, bool) {
    let mut result = Vec::new();
    let parsed = catch_unwind(AssertUnwindSafe(|| {
        let mut lexer = Lexer::new_global_name(log, source);

        result.push(lexer.identifier.string.clone());
        match lexer.token {
            Token::This => lexer.next(),
            Token::Import => {
                lexer.next();
                lexer.expect(Token::Dot);
                result.push(lexer.identifier.string.clone());
                lexer.expect_contextual_keyword(b"meta");
            }
            _ => lexer.expect(Token::Identifier),
        }

        while lexer.token != Token::EndOfFile {
            match lexer.token {
                Token::Dot => {
                    lexer.next();
                    if !lexer.is_identifier_or_keyword() {
                        lexer.expected(Token::Identifier);
                    }
                    result.push(lexer.identifier.string.clone());
                    lexer.next();
                }
                Token::OpenBracket => {
                    lexer.next();
                    result.push(utf16_to_string(lexer.string_literal()));
                    lexer.expect(Token::StringLiteral);
                    lexer.expect(Token::CloseBracket);
                }
                _ => lexer.expected(Token::Dot),
            }
        }
    }));

    match parsed {
        Ok(()) => (result, true),
        Err(payload) if payload.downcast_ref::<LexerPanic>().is_some() => (result, false),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::parse_global_name;
    use crate::internal::logger::{DeferLogKind, Log, Source};

    fn parse(text: &[u8]) -> (Vec<Vec<u8>>, bool, Log) {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(text),
            ..Source::default()
        };
        let (parts, ok) = parse_global_name(log.clone(), source);
        (parts, ok, log)
    }

    #[test]
    fn parses_identifier_this_import_meta_and_index_paths() {
        for (text, expected) in [
            (
                b"window.document.title".as_slice(),
                vec![b"window".to_vec(), b"document".to_vec(), b"title".to_vec()],
            ),
            (
                b"this.console['log']",
                vec![b"this".to_vec(), b"console".to_vec(), b"log".to_vec()],
            ),
            (
                b"import.meta.url",
                vec![b"import".to_vec(), b"meta".to_vec(), b"url".to_vec()],
            ),
            (
                br"global['decoded\u{2d}key']",
                vec![b"global".to_vec(), b"decoded-key".to_vec()],
            ),
        ] {
            let (parts, ok, log) = parse(text);
            assert!(ok, "{text:?}");
            assert_eq!(parts, expected, "{text:?}");
            assert!(log.done().is_empty(), "{text:?}");
        }
    }

    #[test]
    fn rejects_non_global_name_expressions() {
        for text in [
            b"foo..bar".as_slice(),
            b"foo[bar]",
            b"foo()",
            b"foo/bar",
            b"import.other",
        ] {
            let (_, ok, log) = parse(text);
            assert!(!ok, "{text:?}");
            assert!(!log.done().is_empty(), "{text:?}");
        }
    }
}
