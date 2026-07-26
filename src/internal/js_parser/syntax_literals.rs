#![allow(dead_code)]

use crate::internal::{
    compat::JsFeature,
    helpers::utf16_to_string,
    js_ast::{
        ArrayExpr, Expr, ExprData, IdentifierExpr, NameOfSymbolExpr, ObjectExpr, OpCode, Property,
        PropertyKind, SpreadExpr, StringExpr, TemplateExpr, TemplatePart, UnaryExpr,
        is_property_access,
    },
    js_lexer::{CommentBefore, Lexer, MaybeSubstring, Token},
    logger::Loc,
};

use super::parser_core::ParserCore;

pub(crate) fn parse_simple_prefix(core: &mut ParserCore, lexer: &mut Lexer) -> Option<Expr> {
    let loc = lexer.loc();
    let data = match lexer.token {
        Token::False => ExprData::Boolean(false),
        Token::True => ExprData::Boolean(true),
        Token::Null => ExprData::Null,
        Token::This => ExprData::This,
        Token::Identifier => {
            let reference = core.store_name_in_ref(lexer.identifier.clone());
            lexer.next();
            return Some(Expr::new(
                loc,
                ExprData::Identifier(IdentifierExpr {
                    reference,
                    ..IdentifierExpr::default()
                }),
            ));
        }
        _ => return None,
    };
    lexer.next();
    Some(Expr::new(loc, data))
}

pub(crate) fn parse_array_prefix(
    lexer: &mut Lexer,
    mut parse_item: impl FnMut(&mut Lexer) -> Expr,
) -> Option<Expr> {
    if lexer.token != Token::OpenBracket {
        return None;
    }
    let loc = lexer.loc();
    lexer.next();
    let mut is_single_line = !lexer.has_newline_before;
    let mut items = Vec::new();
    let mut comma_after_spread = Loc::default();

    while lexer.token != Token::CloseBracket {
        match lexer.token {
            Token::Comma => {
                items.push(Expr::new(lexer.loc(), ExprData::Missing));
            }
            Token::DotDotDot => {
                let dots_loc = lexer.loc();
                lexer.next();
                let item = parse_item(lexer);
                items.push(Expr::new(
                    dots_loc,
                    ExprData::Spread(SpreadExpr { value: item }),
                ));
                if lexer.token == Token::Comma {
                    comma_after_spread = lexer.loc();
                }
            }
            _ => items.push(parse_item(lexer)),
        }

        if lexer.token != Token::Comma {
            break;
        }
        if lexer.has_newline_before {
            is_single_line = false;
        }
        lexer.next();
        if lexer.has_newline_before {
            is_single_line = false;
        }
    }

    if lexer.has_newline_before {
        is_single_line = false;
    }
    let close_bracket_loc = lexer.loc();
    lexer.expect(Token::CloseBracket);
    Some(Expr::new(
        loc,
        ExprData::Array(ArrayExpr {
            items,
            comma_after_spread,
            close_bracket_loc,
            is_single_line,
            ..ArrayExpr::default()
        }),
    ))
}

pub(crate) fn parse_object_prefix(
    lexer: &mut Lexer,
    mut parse_value: impl FnMut(&mut Lexer) -> Expr,
    mut parse_property: impl FnMut(&mut Lexer) -> Option<Property>,
) -> Option<Expr> {
    if lexer.token != Token::OpenBrace {
        return None;
    }
    let loc = lexer.loc();
    lexer.next();
    let mut is_single_line = !lexer.has_newline_before;
    let mut properties = Vec::new();
    let mut comma_after_spread = Loc::default();

    while lexer.token != Token::CloseBrace {
        if lexer.token == Token::DotDotDot {
            let dot_loc = lexer.loc();
            lexer.next();
            properties.push(Property {
                kind: PropertyKind::Spread,
                loc: dot_loc,
                value_or_nil: parse_value(lexer),
                ..Property::default()
            });
            if lexer.token == Token::Comma {
                comma_after_spread = lexer.loc();
            }
        } else if let Some(property) = parse_property(lexer) {
            properties.push(property);
        }

        if lexer.token != Token::Comma {
            break;
        }
        if lexer.has_newline_before {
            is_single_line = false;
        }
        lexer.next();
        if lexer.has_newline_before {
            is_single_line = false;
        }
    }

    if lexer.has_newline_before {
        is_single_line = false;
    }
    let close_brace_loc = lexer.loc();
    lexer.expect(Token::CloseBrace);
    Some(Expr::new(
        loc,
        ExprData::Object(ObjectExpr {
            properties,
            comma_after_spread,
            close_brace_loc,
            is_single_line,
            ..ObjectExpr::default()
        }),
    ))
}

pub(crate) fn parse_untagged_template_prefix(
    lexer: &mut Lexer,
    mut parse_value: impl FnMut(&mut Lexer) -> Expr,
) -> Option<Expr> {
    if lexer.token != Token::TemplateHead {
        return None;
    }
    let loc = lexer.loc();
    let head_loc = lexer.loc();
    let head_cooked = lexer.string_literal().to_vec();
    let mut legacy_octal_loc = if lexer.legacy_octal_loc.start > loc.start {
        lexer.legacy_octal_loc
    } else {
        Loc::default()
    };
    let mut parts = Vec::new();

    loop {
        lexer.next();
        let value = parse_value(lexer);
        let tail_loc = lexer.loc();
        lexer.rescan_close_brace_as_template_token();
        let tail_cooked = lexer.string_literal().to_vec();
        if lexer.legacy_octal_loc.start > tail_loc.start {
            legacy_octal_loc = lexer.legacy_octal_loc;
        }
        parts.push(TemplatePart {
            value,
            tail_cooked,
            tail_loc,
            ..TemplatePart::default()
        });
        if lexer.token == Token::TemplateTail {
            lexer.next();
            break;
        }
    }

    Some(Expr::new(
        loc,
        ExprData::Template(TemplateExpr {
            head_cooked,
            parts,
            head_loc,
            legacy_octal_loc,
            ..TemplateExpr::default()
        }),
    ))
}

pub(crate) fn parse_tagged_template_suffix(
    tag: Expr,
    lexer: &mut Lexer,
    mut parse_value: impl FnMut(&mut Lexer) -> Expr,
) -> Option<Expr> {
    if !matches!(
        lexer.token,
        Token::NoSubstitutionTemplateLiteral | Token::TemplateHead
    ) {
        return None;
    }
    let loc = tag.loc;
    let head_loc = lexer.loc();
    let (_, head_raw) = lexer.cooked_and_raw_template_contents();
    let head_raw = String::from_utf8(head_raw).expect("template source text must be valid UTF-8");
    let mut parts = Vec::new();

    if lexer.token == Token::NoSubstitutionTemplateLiteral {
        lexer.next();
    } else {
        loop {
            lexer.next();
            let value = parse_value(lexer);
            let tail_loc = lexer.loc();
            lexer.rescan_close_brace_as_template_token();
            let (_, tail_raw) = lexer.cooked_and_raw_template_contents();
            parts.push(TemplatePart {
                value,
                tail_raw: String::from_utf8(tail_raw)
                    .expect("template source text must be valid UTF-8"),
                tail_loc,
                ..TemplatePart::default()
            });
            if lexer.token == Token::TemplateTail {
                lexer.next();
                break;
            }
        }
    }

    Some(Expr::new(
        loc,
        ExprData::Template(TemplateExpr {
            tag_or_nil: tag,
            head_raw,
            parts,
            head_loc,
            ..TemplateExpr::default()
        }),
    ))
}

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
        parse_array_prefix, parse_big_int_or_string_if_unsupported, parse_numeric_literal,
        parse_object_prefix, parse_regular_expression_literal, parse_simple_prefix,
        parse_string_literal, parse_tagged_template_suffix, parse_unary_prefix,
        parse_untagged_template_prefix,
    };
    use crate::internal::{
        config::TsOptions,
        js_ast::{Expr, ExprData, Property, PropertyKind, StringExpr},
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

    #[test]
    fn parses_primitives_and_source_backed_identifier_names() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"identifier + true"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let identifier =
            parse_simple_prefix(&mut core, &mut lexer).expect("expected identifier prefix");
        let Some(ExprData::Identifier(identifier)) = identifier.data.as_deref() else {
            panic!("expected identifier");
        };
        assert_eq!(core.load_name_from_ref(identifier.reference), b"identifier");
        assert_eq!(lexer.token, Token::Plus);
        lexer.next();
        let boolean = parse_simple_prefix(&mut core, &mut lexer).expect("expected boolean prefix");
        assert!(matches!(
            boolean.data.as_deref(),
            Some(ExprData::Boolean(true))
        ));
    }

    #[test]
    fn parses_array_holes_spreads_and_trailing_commas() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"[, 1, ...2,]"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source, TsOptions::default());
        let array = parse_array_prefix(&mut lexer, |lexer| {
            let loc = lexer.loc();
            let number = lexer.number;
            lexer.next();
            crate::internal::js_ast::Expr::new(loc, ExprData::Number(number))
        })
        .expect("expected array");
        let Some(ExprData::Array(array)) = array.data.as_deref() else {
            panic!("expected array expression");
        };
        assert_eq!(array.items.len(), 3);
        assert!(matches!(
            array.items[0].data.as_deref(),
            Some(ExprData::Missing)
        ));
        assert!(matches!(
            array.items[2].data.as_deref(),
            Some(ExprData::Spread(_))
        ));
        assert!(array.comma_after_spread.start > 0);
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn parses_object_properties_spreads_and_layout_metadata() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"{a: 1, ...2,}"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source, TsOptions::default());
        let parse_number = |lexer: &mut Lexer| {
            let loc = lexer.loc();
            let value = lexer.number;
            lexer.next();
            Expr::new(loc, ExprData::Number(value))
        };
        let object = parse_object_prefix(&mut lexer, parse_number, |lexer| {
            let loc = lexer.loc();
            let name = lexer.identifier.string.clone();
            lexer.next();
            lexer.expect(Token::Colon);
            let value_loc = lexer.loc();
            let value = lexer.number;
            lexer.next();
            Some(Property {
                key: Expr::new(
                    loc,
                    ExprData::String(StringExpr {
                        value: String::from_utf8(name)
                            .expect("identifier is UTF-8")
                            .encode_utf16()
                            .collect(),
                        ..StringExpr::default()
                    }),
                ),
                value_or_nil: Expr::new(value_loc, ExprData::Number(value)),
                kind: PropertyKind::Field,
                ..Property::default()
            })
        })
        .expect("expected object");
        let Some(ExprData::Object(object)) = object.data.as_deref() else {
            panic!("expected object expression");
        };
        assert_eq!(object.properties.len(), 2);
        assert_eq!(object.properties[1].kind, PropertyKind::Spread);
        assert!(object.comma_after_spread.start > 0);
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn parses_untagged_template_parts_and_advances_after_tail() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"`a${1}b${2}c` + 3"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source, TsOptions::default());
        let template = parse_untagged_template_prefix(&mut lexer, |lexer| {
            let loc = lexer.loc();
            let value = lexer.number;
            lexer.next();
            Expr::new(loc, ExprData::Number(value))
        })
        .expect("expected template");
        let Some(ExprData::Template(template)) = template.data.as_deref() else {
            panic!("expected template expression");
        };
        assert_eq!(template.head_cooked, "a".encode_utf16().collect::<Vec<_>>());
        assert_eq!(template.parts.len(), 2);
        assert_eq!(
            template.parts[0].tail_cooked,
            "b".encode_utf16().collect::<Vec<_>>()
        );
        assert_eq!(
            template.parts[1].tail_cooked,
            "c".encode_utf16().collect::<Vec<_>>()
        );
        assert_eq!(lexer.token, Token::Plus);
    }

    #[test]
    fn tagged_templates_preserve_raw_invalid_escapes() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&br"tag`\xZ${1}\r` + 2"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let tag = parse_simple_prefix(&mut core, &mut lexer).expect("expected tag");
        let template = parse_tagged_template_suffix(tag, &mut lexer, |lexer| {
            let loc = lexer.loc();
            let value = lexer.number;
            lexer.next();
            Expr::new(loc, ExprData::Number(value))
        })
        .expect("expected tagged template");
        let Some(ExprData::Template(template)) = template.data.as_deref() else {
            panic!("expected template expression");
        };
        assert_eq!(template.head_raw, r"\xZ");
        assert_eq!(template.parts[0].tail_raw, r"\r");
        assert!(template.tag_or_nil.data.is_some());
        assert_eq!(lexer.token, Token::Plus);
    }
}
