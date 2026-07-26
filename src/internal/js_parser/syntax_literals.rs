#![allow(dead_code)]

use crate::internal::{
    compat::JsFeature,
    helpers::utf16_to_string,
    js_ast::{Expr, ExprData, NameOfSymbolExpr, OpCode, StringExpr, UnaryExpr, is_property_access},
    js_lexer::{CommentBefore, Lexer, MaybeSubstring, Token},
    logger::Loc,
};

use super::parser_core::ParserCore;

pub(crate) fn parse_big_int_or_string_if_unsupported(core: &ParserCore, lexer: &Lexer) -> Expr {
    let loc = lexer.loc();
    let text = std::str::from_utf8(&lexer.identifier.string)
        .expect("big integer tokens must be valid ASCII");
    if core
        .options
        .unsupported_js_features
        .contains(JsFeature::BIGINT)
    {
        let (digits, radix) = if let Some(digits) = text.strip_prefix("0b") {
            (digits, 2)
        } else if let Some(digits) = text.strip_prefix("0o") {
            (digits, 8)
        } else if let Some(digits) = text.strip_prefix("0x") {
            (digits, 16)
        } else {
            (text, 10)
        };
        let decimal = num_bigint::BigUint::parse_bytes(digits.as_bytes(), radix)
            .expect("lexer only produces valid big integer tokens")
            .to_str_radix(10);
        Expr::new(
            loc,
            ExprData::String(StringExpr {
                value: decimal.encode_utf16().collect(),
                ..StringExpr::default()
            }),
        )
    } else {
        Expr::new(loc, ExprData::BigInt(text.into()))
    }
}

pub(crate) fn parse_numeric_literal(core: &mut ParserCore, lexer: &mut Lexer) -> Expr {
    let loc = lexer.loc();
    let value = Expr::new(loc, ExprData::Number(lexer.number));
    if lexer.is_legacy_octal_literal {
        core.legacy_octal_literals.insert(loc, lexer.range());
    }
    lexer.next();
    value
}

pub(crate) fn parse_regular_expression_literal(lexer: &mut Lexer) -> Expr {
    let loc = lexer.loc();
    lexer.scan_reg_exp();
    let value = String::from_utf8(lexer.raw().to_vec())
        .expect("regular expression source must be valid UTF-8");
    lexer.next();
    Expr::new(loc, ExprData::RegExp(value))
}

pub(crate) fn parse_unary_prefix(
    lexer: &mut Lexer,
    mut parse_operand: impl FnMut(&mut Lexer) -> Expr,
) -> Option<Expr> {
    let loc = lexer.loc();
    let (op, check_exponentiation) = match lexer.token {
        Token::Void => (OpCode::UnaryVoid, true),
        Token::Typeof => (OpCode::UnaryTypeof, true),
        Token::Delete => (OpCode::UnaryDelete, true),
        Token::Plus => (OpCode::UnaryPositive, true),
        Token::Minus => (OpCode::UnaryNegative, true),
        Token::Tilde => (OpCode::UnaryComplement, true),
        Token::Exclamation => (OpCode::UnaryNot, true),
        Token::MinusMinus => (OpCode::UnaryPreDecrement, false),
        Token::PlusPlus => (OpCode::UnaryPreIncrement, false),
        _ => return None,
    };
    lexer.next();
    let value = parse_operand(lexer);
    if check_exponentiation && lexer.token == Token::AsteriskAsterisk {
        lexer.unexpected();
    }
    let was_identifier = matches!(value.data.as_deref(), Some(ExprData::Identifier(_)));
    let was_property_access = is_property_access(&value);
    Some(Expr::new(
        loc,
        ExprData::Unary(UnaryExpr {
            value,
            op,
            was_originally_typeof_identifier: op == OpCode::UnaryTypeof && was_identifier,
            was_originally_delete_of_identifier_or_property_access: op == OpCode::UnaryDelete
                && (was_identifier || was_property_access),
        }),
    ))
}

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

    use super::{
        parse_big_int_or_string_if_unsupported, parse_numeric_literal,
        parse_regular_expression_literal, parse_string_literal, parse_unary_prefix,
    };
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

    #[test]
    fn normalizes_unsupported_bigints_to_decimal_strings() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"0x1_0000_0000_0000_0001n"[..]),
            ..Source::default()
        };
        let lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let options = Options {
            unsupported_js_features: crate::internal::compat::JsFeature::BIGINT,
            ..Options::default()
        };
        let core = super::ParserCore::new(source, options);
        let expr = parse_big_int_or_string_if_unsupported(&core, &lexer);
        assert!(matches!(
            expr.data.as_deref(),
            Some(ExprData::String(value))
                if value.value == "18446744073709551617".encode_utf16().collect::<Vec<_>>()
        ));
    }

    #[test]
    fn parses_numbers_and_tracks_legacy_octal_literals() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"0123 + 1"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let expr = parse_numeric_literal(&mut core, &mut lexer);
        assert!(matches!(
            expr.data.as_deref(),
            Some(ExprData::Number(value)) if value.to_bits() == 83.0_f64.to_bits()
        ));
        assert_eq!(core.legacy_octal_literals.len(), 1);
        assert_eq!(lexer.token, Token::Plus);
    }

    #[test]
    fn rescans_slash_tokens_as_regular_expressions() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&br"/a[b/]c/gi + 1"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source, TsOptions::default());
        let expr = parse_regular_expression_literal(&mut lexer);
        assert!(matches!(
            expr.data.as_deref(),
            Some(ExprData::RegExp(value)) if value == "/a[b/]c/gi"
        ));
        assert_eq!(lexer.token, Token::Plus);
    }

    #[test]
    fn parses_unary_prefix_metadata() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"typeof 1 + 2"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source, TsOptions::default());
        let expr = parse_unary_prefix(&mut lexer, |lexer| {
            let loc = lexer.loc();
            let value = lexer.number;
            lexer.next();
            crate::internal::js_ast::Expr::new(loc, ExprData::Number(value))
        })
        .expect("expected unary prefix");
        let Some(ExprData::Unary(unary)) = expr.data.as_deref() else {
            panic!("expected unary expression");
        };
        assert_eq!(unary.op, crate::internal::js_ast::OpCode::UnaryTypeof);
        assert!(!unary.was_originally_typeof_identifier);
        assert_eq!(lexer.token, Token::Plus);
    }
}
