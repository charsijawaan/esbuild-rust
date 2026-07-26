#![allow(dead_code)]

use crate::internal::{
    helpers::string_to_utf16,
    js_ast::{
        ArrayBinding, ArrayBindingPattern, Binding, BindingData, Expr, ExprData, IdentifierBinding,
        NameOfSymbolExpr, ObjectBindingPattern, Precedence, PropertyBinding, StringExpr,
    },
    js_lexer::{Lexer, MaybeSubstring, Token},
    logger::Loc,
};

use super::{
    parser_core::ParserCore,
    parser_types::AwaitOrYield,
    syntax_expression::parse_expression,
    syntax_literals::{
        parse_big_int_or_string_if_unsupported, parse_numeric_literal, parse_string_literal,
    },
};

pub(crate) fn parse_binding(core: &mut ParserCore, lexer: &mut Lexer) -> Binding {
    let loc = lexer.loc();
    match lexer.token {
        Token::Identifier => {
            let name = lexer.identifier.clone();
            let text = String::from_utf8(name.string.clone())
                .expect("binding identifiers must be valid UTF-8");
            let invalid = (text == "await"
                && core.fn_or_arrow_data_parse.await_policy != AwaitOrYield::AllowIdentifier)
                || (text == "yield"
                    && core.fn_or_arrow_data_parse.yield_policy != AwaitOrYield::AllowIdentifier);
            if invalid {
                core.add_error_range(
                    lexer.range(),
                    format!("Cannot use \"{text}\" as an identifier here:"),
                );
            }
            let reference = core.store_name_in_ref(name);
            lexer.next();
            Binding {
                loc,
                data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                    reference,
                }))),
            }
        }
        Token::OpenBracket => parse_array_binding(core, lexer),
        Token::OpenBrace => parse_object_binding(core, lexer),
        _ => {
            lexer.expected(Token::Identifier);
        }
    }
}

fn parse_array_binding(core: &mut ParserCore, lexer: &mut Lexer) -> Binding {
    let loc = lexer.loc();
    lexer.expect(Token::OpenBracket);
    let mut is_single_line = !lexer.has_newline_before;
    let mut items = Vec::new();
    let mut has_spread = false;
    while lexer.token != Token::CloseBracket {
        let item_loc = lexer.loc();
        if lexer.token == Token::Comma {
            items.push(ArrayBinding {
                binding: Binding {
                    loc: item_loc,
                    data: Some(Box::new(BindingData::Missing)),
                },
                loc: item_loc,
                ..ArrayBinding::default()
            });
        } else {
            if lexer.token == Token::DotDotDot {
                lexer.next();
                has_spread = true;
            }
            let binding = parse_binding(core, lexer);
            let default_value_or_nil = if !has_spread && lexer.token == Token::Equals {
                lexer.next();
                parse_expression(core, lexer, Precedence::Comma, true)
            } else {
                Expr::default()
            };
            items.push(ArrayBinding {
                binding,
                default_value_or_nil,
                loc: item_loc,
            });
            if has_spread && lexer.token == Token::Comma {
                core.add_error_range(lexer.range(), "Unexpected \",\" after rest pattern");
            }
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
    Binding {
        loc,
        data: Some(Box::new(BindingData::Array(ArrayBindingPattern {
            items,
            close_bracket_loc,
            has_spread,
            is_single_line,
        }))),
    }
}

fn parse_object_binding(core: &mut ParserCore, lexer: &mut Lexer) -> Binding {
    let loc = lexer.loc();
    lexer.expect(Token::OpenBrace);
    let mut is_single_line = !lexer.has_newline_before;
    let mut properties = Vec::new();
    while lexer.token != Token::CloseBrace {
        let property = parse_property_binding(core, lexer);
        let is_spread = property.is_spread;
        properties.push(property);
        if is_spread && lexer.token == Token::Comma {
            core.add_error_range(lexer.range(), "Unexpected \",\" after rest pattern");
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
    Binding {
        loc,
        data: Some(Box::new(BindingData::Object(ObjectBindingPattern {
            properties,
            close_brace_loc,
            is_single_line,
        }))),
    }
}

#[allow(clippy::too_many_lines)]
fn parse_property_binding(core: &mut ParserCore, lexer: &mut Lexer) -> PropertyBinding {
    let loc = lexer.loc();
    if lexer.token == Token::DotDotDot {
        lexer.next();
        return PropertyBinding {
            value: parse_binding(core, lexer),
            loc,
            is_spread: true,
            ..PropertyBinding::default()
        };
    }

    let mut is_computed = false;
    let mut prefer_quoted_key = false;
    let mut close_bracket_loc = Loc::default();
    let mut shorthand = None;
    let key = match lexer.token {
        Token::NumericLiteral => parse_numeric_literal(core, lexer),
        Token::StringLiteral => {
            let key = parse_string_literal(core, lexer);
            prefer_quoted_key = !core.options.minify_syntax;
            key
        }
        Token::BigIntegerLiteral => {
            let key = parse_big_int_or_string_if_unsupported(core, lexer);
            lexer.next();
            key
        }
        Token::OpenBracket => {
            is_computed = true;
            lexer.next();
            let key = parse_expression(core, lexer, Precedence::Comma, true);
            close_bracket_loc = lexer.loc();
            lexer.expect(Token::CloseBracket);
            key
        }
        _ => {
            if !lexer.is_identifier_or_keyword() {
                lexer.expected(Token::Identifier);
            }
            let name = lexer.identifier.clone();
            let name_loc = lexer.loc();
            let is_identifier = lexer.token == Token::Identifier;
            lexer.next();
            if is_identifier && lexer.token != Token::Colon && lexer.token != Token::OpenParen {
                let value = Binding {
                    loc: name_loc,
                    data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                        reference: core.store_name_in_ref(name.clone()),
                    }))),
                };
                let default_value_or_nil = if lexer.token == Token::Equals {
                    lexer.next();
                    parse_expression(core, lexer, Precedence::Comma, true)
                } else {
                    Expr::default()
                };
                shorthand = Some((value, default_value_or_nil));
            }
            binding_property_name(core, name_loc, name)
        }
    };

    if let Some((value, default_value_or_nil)) = shorthand {
        return PropertyBinding {
            key,
            value,
            default_value_or_nil,
            loc,
            ..PropertyBinding::default()
        };
    }

    lexer.expect(Token::Colon);
    let value = parse_binding(core, lexer);
    let default_value_or_nil = if lexer.token == Token::Equals {
        lexer.next();
        parse_expression(core, lexer, Precedence::Comma, true)
    } else {
        Expr::default()
    };
    PropertyBinding {
        key,
        value,
        default_value_or_nil,
        loc,
        close_bracket_loc,
        is_computed,
        prefer_quoted_key,
        ..PropertyBinding::default()
    }
}

fn binding_property_name(core: &mut ParserCore, loc: Loc, name: MaybeSubstring) -> Expr {
    let text =
        String::from_utf8(name.string.clone()).expect("binding property names must be valid UTF-8");
    if core.is_mangled_prop(&text) {
        Expr::new(
            loc,
            ExprData::NameOfSymbol(NameOfSymbolExpr {
                reference: core.store_name_in_ref(name),
                ..NameOfSymbolExpr::default()
            }),
        )
    } else {
        Expr::new(
            loc,
            ExprData::String(StringExpr {
                value: string_to_utf16(text.as_bytes()),
                ..StringExpr::default()
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::parse_binding;
    use crate::internal::{
        config::TsOptions,
        js_ast::BindingData,
        js_lexer::{Lexer, Token},
        js_parser::Options,
        logger::{DeferLogKind, Log, Source},
    };

    #[test]
    fn parses_nested_array_and_object_binding_patterns() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"[first, {value: renamed = 1}, ...rest]"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let binding = parse_binding(&mut core, &mut lexer);
        let Some(BindingData::Array(array)) = binding.data.as_deref() else {
            panic!("expected array binding");
        };
        assert_eq!(array.items.len(), 3);
        assert!(array.has_spread);
        assert!(matches!(
            array.items[1].binding.data.as_deref(),
            Some(BindingData::Object(_))
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }
}
