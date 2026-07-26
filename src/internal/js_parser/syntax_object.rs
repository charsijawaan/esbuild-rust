#![allow(dead_code)]

use crate::internal::{
    helpers::string_to_utf16,
    js_ast::{
        Expr, ExprData, FunctionExpr, IdentifierExpr, NameOfSymbolExpr, ObjectExpr, Precedence,
        Property, PropertyFlags, PropertyKind, StringExpr,
    },
    js_lexer::{Lexer, Token},
    logger::Loc,
};

use super::{
    parser_core::ParserCore,
    parser_types::{AwaitOrYield, FnOrArrowDataParse},
    syntax_function::parse_function_tail,
    syntax_literals::{
        parse_big_int_or_string_if_unsupported, parse_numeric_literal, parse_string_literal,
    },
};

pub(crate) fn parse_object_literal_prefix(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    mut parse_expression: impl FnMut(&mut ParserCore, &mut Lexer, Precedence) -> Expr,
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
                value_or_nil: parse_expression(core, lexer, Precedence::Comma),
                ..Property::default()
            });
            if lexer.token == Token::Comma {
                comma_after_spread = lexer.loc();
            }
        } else {
            properties.push(parse_property(core, lexer, &mut parse_expression));
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

#[allow(clippy::too_many_lines)]
fn parse_property(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    parse_expression: &mut impl FnMut(&mut ParserCore, &mut Lexer, Precedence) -> Expr,
) -> Property {
    let start_loc = lexer.loc();
    let mut kind = PropertyKind::Field;
    let mut is_async = false;
    let mut preconsumed_identifier = None;

    if lexer.token == Token::Identifier && matches!(lexer.raw(), b"get" | b"set" | b"async") {
        let name = lexer.identifier.clone();
        let name_range = lexer.range();
        let name_loc = lexer.loc();
        let modifier = lexer.raw().to_vec();
        lexer.next();
        let could_be_modifier = lexer.is_identifier_or_keyword()
            || matches!(
                lexer.token,
                Token::OpenBracket
                    | Token::NumericLiteral
                    | Token::StringLiteral
                    | Token::PrivateIdentifier
            )
            || (modifier == b"async" && lexer.token == Token::Asterisk);
        if could_be_modifier && (modifier != b"async" || !lexer.has_newline_before) {
            match modifier.as_slice() {
                b"get" => kind = PropertyKind::Getter,
                b"set" => kind = PropertyKind::Setter,
                b"async" => is_async = true,
                _ => unreachable!(),
            }
        } else {
            preconsumed_identifier = Some((name, name_range, name_loc));
        }
    }

    let is_generator = preconsumed_identifier.is_none() && lexer.token == Token::Asterisk;
    if is_generator {
        lexer.next();
    }
    let key_loc = preconsumed_identifier
        .as_ref()
        .map_or_else(|| lexer.loc(), |(_, _, loc)| *loc);
    let key_range = preconsumed_identifier
        .as_ref()
        .map_or_else(|| lexer.range(), |(_, range, _)| *range);
    let mut flags = PropertyFlags::NONE;
    let mut close_bracket_loc = Loc::default();
    let mut shorthand = None;

    let key = if let Some((name, name_range, name_loc)) = preconsumed_identifier {
        let (key, value, identifier_flags) =
            parse_identifier_property_key(core, lexer, name, name_range, name_loc, true);
        shorthand = value;
        flags |= identifier_flags;
        key
    } else {
        match lexer.token {
            Token::NumericLiteral => parse_numeric_literal(core, lexer),
            Token::StringLiteral => {
                let key = parse_string_literal(core, lexer);
                if !core.options.minify_syntax {
                    flags |= PropertyFlags::PREFER_QUOTED_KEY;
                }
                key
            }
            Token::BigIntegerLiteral => {
                let key = parse_big_int_or_string_if_unsupported(core, lexer);
                lexer.next();
                key
            }
            Token::PrivateIdentifier => {
                lexer.expected(Token::Identifier);
            }
            Token::OpenBracket => {
                flags |= PropertyFlags::IS_COMPUTED;
                lexer.next();
                let key = parse_expression(core, lexer, Precedence::Comma);
                close_bracket_loc = lexer.loc();
                lexer.expect(Token::CloseBracket);
                key
            }
            _ => {
                if !lexer.is_identifier_or_keyword() {
                    lexer.expected(Token::Identifier);
                }
                let name = lexer.identifier.clone();
                let name_range = lexer.range();
                let was_identifier = lexer.token == Token::Identifier;
                lexer.next();
                let (key, value, identifier_flags) = parse_identifier_property_key(
                    core,
                    lexer,
                    name,
                    name_range,
                    key_loc,
                    was_identifier,
                );
                shorthand = value;
                flags |= identifier_flags;
                key
            }
        }
    };

    if lexer.token == Token::OpenParen || kind.is_method_definition() {
        let mut function = parse_function_tail(
            core,
            lexer,
            None,
            false,
            false,
            FnOrArrowDataParse {
                await_policy: if is_async {
                    AwaitOrYield::AllowExpression
                } else {
                    AwaitOrYield::AllowIdentifier
                },
                yield_policy: if is_generator {
                    AwaitOrYield::AllowExpression
                } else {
                    AwaitOrYield::AllowIdentifier
                },
                allow_super_property: true,
                ..FnOrArrowDataParse::default()
            },
        );
        function.is_unique_formal_parameters = true;
        if kind == PropertyKind::Getter && !function.args.is_empty() {
            core.add_error_range(
                key_range,
                format!(
                    "Getter {} must have zero arguments",
                    key_name_for_error(&key)
                ),
            );
        } else if kind == PropertyKind::Setter && function.args.len() != 1 {
            core.add_error_range(
                key_range,
                format!(
                    "Setter {} must have exactly one argument",
                    key_name_for_error(&key)
                ),
            );
        } else if kind == PropertyKind::Field {
            kind = PropertyKind::Method;
        }
        return Property {
            key,
            value_or_nil: Expr::new(
                start_loc,
                ExprData::Function(FunctionExpr {
                    function,
                    ..FunctionExpr::default()
                }),
            ),
            loc: start_loc,
            close_bracket_loc,
            kind,
            flags,
            ..Property::default()
        };
    }
    if is_generator {
        lexer.expected(Token::OpenParen);
    }

    let value_or_nil = if let Some(value) = shorthand {
        value
    } else {
        lexer.expect(Token::Colon);
        parse_expression(core, lexer, Precedence::Comma)
    };

    Property {
        key,
        value_or_nil,
        loc: start_loc,
        close_bracket_loc,
        kind: PropertyKind::Field,
        flags,
        ..Property::default()
    }
}

fn parse_identifier_property_key(
    core: &mut ParserCore,
    lexer: &Lexer,
    name: crate::internal::js_lexer::MaybeSubstring,
    name_range: crate::internal::logger::Range,
    name_loc: Loc,
    was_identifier: bool,
) -> (Expr, Option<Expr>, PropertyFlags) {
    let mut shorthand = None;
    let mut flags = PropertyFlags::NONE;
    if was_identifier
        && lexer.token != Token::Colon
        && lexer.token != Token::OpenParen
        && lexer.token != Token::LessThan
    {
        let name_text = String::from_utf8(name.string.clone())
            .expect("identifier property names must be valid UTF-8");
        let invalid_contextual_name = (name_text == "await"
            && core.fn_or_arrow_data_parse.await_policy != AwaitOrYield::AllowIdentifier)
            || (name_text == "yield"
                && core.fn_or_arrow_data_parse.yield_policy != AwaitOrYield::AllowIdentifier);
        if invalid_contextual_name {
            core.add_error_range(
                name_range,
                format!("Cannot use \"{name_text}\" as an identifier here:"),
            );
        }
        shorthand = Some(Expr::new(
            name_loc,
            ExprData::Identifier(IdentifierExpr {
                reference: core.store_name_in_ref(name.clone()),
                ..IdentifierExpr::default()
            }),
        ));
        flags |= PropertyFlags::WAS_SHORTHAND;
    }

    (property_name_expr(core, name_loc, name), shorthand, flags)
}

fn key_name_for_error(key: &Expr) -> String {
    if let Some(ExprData::String(string)) = key.data.as_deref() {
        format!(
            "{:?}",
            String::from_utf8_lossy(&crate::internal::helpers::utf16_to_string(&string.value))
        )
    } else {
        "property".into()
    }
}

fn property_name_expr(
    core: &mut ParserCore,
    loc: Loc,
    name: crate::internal::js_lexer::MaybeSubstring,
) -> Expr {
    let name_text =
        String::from_utf8(name.string.clone()).expect("property names must be valid UTF-8");
    if core.is_mangled_prop(&name_text) {
        Expr::new(
            loc,
            ExprData::NameOfSymbol(NameOfSymbolExpr {
                reference: core.store_name_in_ref(name),
                has_property_key_comment: true,
            }),
        )
    } else {
        Expr::new(
            loc,
            ExprData::String(StringExpr {
                value: string_to_utf16(name_text.as_bytes()),
                ..StringExpr::default()
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::parse_object_literal_prefix;
    use crate::internal::{
        config::TsOptions,
        js_ast::{Expr, ExprData, Precedence, PropertyFlags, PropertyKind},
        js_lexer::{Lexer, Token},
        js_parser::Options,
        logger::{DeferLogKind, Log, Source},
    };

    fn parse_number(_: &mut super::ParserCore, lexer: &mut Lexer, _: Precedence) -> Expr {
        let loc = lexer.loc();
        let value = lexer.number;
        lexer.next();
        Expr::new(loc, ExprData::Number(value))
    }

    #[test]
    fn parses_data_computed_shorthand_and_spread_properties() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"{a: 1, [2]: 3, shorthand, ...4}"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let object =
            parse_object_literal_prefix(&mut core, &mut lexer, parse_number).expect("object");
        let Some(ExprData::Object(object)) = object.data.as_deref() else {
            panic!("expected object");
        };
        assert_eq!(object.properties.len(), 4);
        assert!(
            object.properties[1]
                .flags
                .contains(PropertyFlags::IS_COMPUTED)
        );
        assert!(
            object.properties[2]
                .flags
                .contains(PropertyFlags::WAS_SHORTHAND)
        );
        assert_eq!(object.properties[3].kind, PropertyKind::Spread);
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn parses_ordinary_and_generator_methods() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"{method(a) { return a }, *generator() { yield 1 }}"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let object =
            parse_object_literal_prefix(&mut core, &mut lexer, parse_number).expect("object");
        let Some(ExprData::Object(object)) = object.data.as_deref() else {
            panic!("expected object");
        };
        assert_eq!(object.properties.len(), 2);
        assert!(
            object
                .properties
                .iter()
                .all(|property| property.kind == PropertyKind::Method)
        );
        assert!(matches!(
            object.properties[1].value_or_nil.data.as_deref(),
            Some(ExprData::Function(function)) if function.function.is_generator
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn parses_accessors_async_methods_and_async_generators() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                &b"{get value() { return 1 }, set value(v) { this._v = v }, async load() { await work }, async *stream() { yield await item }}"[..],
            ),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let object =
            parse_object_literal_prefix(&mut core, &mut lexer, parse_number).expect("object");
        let Some(ExprData::Object(object)) = object.data.as_deref() else {
            panic!("expected object");
        };
        assert_eq!(object.properties[0].kind, PropertyKind::Getter);
        assert_eq!(object.properties[1].kind, PropertyKind::Setter);
        assert!(matches!(
            object.properties[2].value_or_nil.data.as_deref(),
            Some(ExprData::Function(function))
                if function.function.is_async && !function.function.is_generator
        ));
        assert!(matches!(
            object.properties[3].value_or_nil.data.as_deref(),
            Some(ExprData::Function(function))
                if function.function.is_async && function.function.is_generator
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn reports_invalid_accessor_arity() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"{get value(arg) {}, set value() {}}"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log.clone(), source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new_with_log(source, Options::default(), log.clone());
        let _ = parse_object_literal_prefix(&mut core, &mut lexer, parse_number).expect("object");
        assert_eq!(log.peek().len(), 2);
    }
}
