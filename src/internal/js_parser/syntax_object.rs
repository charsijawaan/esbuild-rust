#![allow(dead_code)]

use crate::internal::{
    helpers::string_to_utf16,
    js_ast::{
        Expr, ExprData, IdentifierExpr, NameOfSymbolExpr, ObjectExpr, Precedence, Property,
        PropertyFlags, PropertyKind, StringExpr,
    },
    js_lexer::{Lexer, Token},
    logger::Loc,
};

use super::{
    parser_core::ParserCore,
    parser_types::AwaitOrYield,
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

fn parse_property(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    parse_expression: &mut impl FnMut(&mut ParserCore, &mut Lexer, Precedence) -> Expr,
) -> Property {
    let start_loc = lexer.loc();
    let mut flags = PropertyFlags::NONE;
    let mut close_bracket_loc = Loc::default();
    let mut shorthand = None;

    let key = match lexer.token {
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
                        && core.fn_or_arrow_data_parse.yield_policy
                            != AwaitOrYield::AllowIdentifier);
                if invalid_contextual_name {
                    core.add_error_range(
                        name_range,
                        format!("Cannot use \"{name_text}\" as an identifier here:"),
                    );
                }
                shorthand = Some(Expr::new(
                    start_loc,
                    ExprData::Identifier(IdentifierExpr {
                        reference: core.store_name_in_ref(name.clone()),
                        ..IdentifierExpr::default()
                    }),
                ));
                flags |= PropertyFlags::WAS_SHORTHAND;
            }

            property_name_expr(core, start_loc, name)
        }
    };

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
}
