#![allow(dead_code)]

use crate::internal::{
    helpers::utf16_to_string,
    js_ast::{Expr, ExprData, NameOfSymbolExpr, StringExpr},
    js_lexer::{CommentBefore, Lexer, MaybeSubstring, Token},
    logger::Loc,
};

use super::parser_core::ParserCore;

pub(crate) fn parse_string_literal(core: &mut ParserCore, lexer: &mut Lexer) -> Expr {
    let loc = lexer.loc();
    let text = lexer.string_literal().to_vec();
    let prefer_template = lexer.token == Token::NoSubstitutionTemplateLiteral;
    let has_property_key_comment = lexer.has_comment_before.contains(CommentBefore::KEY);

    if has_property_key_comment {
        let name_bytes = utf16_to_string(&text);
        let name = String::from_utf8(name_bytes.clone())
            .expect("identifier-like property strings must be valid UTF-8");
        if core.is_mangled_prop(&name) {
            let value = Expr::new(
                loc,
                ExprData::NameOfSymbol(NameOfSymbolExpr {
                    reference: core.store_name_in_ref(MaybeSubstring::from_allocated(name_bytes)),
                    has_property_key_comment: true,
                }),
            );
            lexer.next();
            return value;
        }
    }

    let legacy_octal_loc = if lexer.legacy_octal_loc.start > loc.start {
        lexer.legacy_octal_loc
    } else {
        Loc::default()
    };
    let value = Expr::new(
        loc,
        ExprData::String(StringExpr {
            value: text,
            legacy_octal_loc,
            prefer_template,
            has_property_key_comment,
            ..StringExpr::default()
        }),
    );
    lexer.next();
    value
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use regex::Regex;

    use super::parse_string_literal;
    use crate::internal::{
        config::TsOptions,
        js_ast::ExprData,
        js_lexer::{Lexer, Token},
        js_parser::Options,
        logger::{DeferLogKind, Log, Source},
    };

    #[test]
    fn parses_strings_and_advances_the_lexer() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&br#""value" + 1"#[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let expr = parse_string_literal(&mut core, &mut lexer);
        assert!(matches!(
            expr.data.as_deref(),
            Some(ExprData::String(value))
                if value.value == "value".encode_utf16().collect::<Vec<_>>()
        ));
        assert_eq!(lexer.token, Token::Plus);
    }

    #[test]
    fn property_key_comments_create_mangled_name_symbols() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&br#"/* @__KEY__ */ "_value""#[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let options = Options {
            mangle_props: Some(Arc::new(
                Regex::new("^_").expect("valid regular expression"),
            )),
            ..Options::default()
        };
        let mut core = super::ParserCore::new(source, options);
        let expr = parse_string_literal(&mut core, &mut lexer);
        assert!(matches!(
            expr.data.as_deref(),
            Some(ExprData::NameOfSymbol(value)) if value.has_property_key_comment
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }
}
