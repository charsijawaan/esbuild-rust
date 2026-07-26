#![allow(dead_code)]

use crate::internal::{
    ast::LocRef,
    helpers::string_to_utf16,
    js_ast::{
        Class, ClassExpr, ClassStaticBlock, Expr, ExprData, FunctionExpr, NameOfSymbolExpr,
        Precedence, PrivateIdentifierExpr, Property, PropertyFlags, PropertyKind, StringExpr,
    },
    js_lexer::{Lexer, MaybeSubstring, Token},
    logger::Loc,
};

use super::{
    parser_core::ParserCore,
    parser_types::{AwaitOrYield, FnOrArrowDataParse},
    syntax_expression::parse_expression,
    syntax_function::parse_function_tail,
    syntax_literals::{
        parse_big_int_or_string_if_unsupported, parse_numeric_literal, parse_string_literal,
    },
    syntax_statement::parse_block,
};

pub(crate) fn parse_class_prefix(core: &mut ParserCore, lexer: &mut Lexer) -> Option<Expr> {
    if lexer.token != Token::Class {
        return None;
    }
    let loc = lexer.loc();
    let class_keyword = lexer.range();
    lexer.next();

    let name = if lexer.token == Token::Identifier {
        let name = LocRef {
            loc: lexer.loc(),
            reference: core.store_name_in_ref(lexer.identifier.clone()),
        };
        lexer.next();
        Some(name)
    } else {
        None
    };
    let extends_or_nil = if lexer.token == Token::Extends {
        lexer.next();
        parse_expression(core, lexer, Precedence::New, true)
    } else {
        Expr::default()
    };

    let body_loc = lexer.loc();
    lexer.expect(Token::OpenBrace);
    let mut properties = Vec::new();
    while lexer.token != Token::CloseBrace {
        if lexer.token == Token::Semicolon {
            lexer.next();
            continue;
        }
        properties.push(parse_class_property(
            core,
            lexer,
            extends_or_nil.data.is_some(),
        ));
    }
    let close_brace_loc = lexer.loc();
    lexer.expect(Token::CloseBrace);
    Some(Expr::new(
        loc,
        ExprData::Class(ClassExpr {
            class: Class {
                name,
                extends_or_nil,
                properties,
                class_keyword,
                body_loc,
                close_brace_loc,
                ..Class::default()
            },
        }),
    ))
}

#[allow(clippy::too_many_lines)]
fn parse_class_property(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    class_has_extends: bool,
) -> Property {
    let start_loc = lexer.loc();
    let mut is_static = false;
    let mut preconsumed_static = None;
    if lexer.is_contextual_keyword(b"static") {
        let name = lexer.identifier.clone();
        let name_loc = lexer.loc();
        lexer.next();
        if lexer.token == Token::OpenBrace {
            let old_context = core.fn_or_arrow_data_parse;
            core.fn_or_arrow_data_parse = FnOrArrowDataParse {
                is_return_disallowed: true,
                allow_super_property: true,
                ..FnOrArrowDataParse::default()
            };
            let (block_loc, block) = parse_block(core, lexer);
            core.fn_or_arrow_data_parse = old_context;
            return Property {
                class_static_block: Some(Box::new(ClassStaticBlock {
                    block,
                    loc: block_loc,
                })),
                loc: start_loc,
                kind: PropertyKind::ClassStaticBlock,
                ..Property::default()
            };
        }
        if lexer.token == Token::OpenParen {
            preconsumed_static = Some((name, name_loc));
        } else {
            is_static = true;
        }
    }

    let is_generator = preconsumed_static.is_none() && lexer.token == Token::Asterisk;
    if is_generator {
        lexer.next();
    }
    let key_loc = preconsumed_static
        .as_ref()
        .map_or_else(|| lexer.loc(), |(_, loc)| *loc);
    let mut flags = if is_static {
        PropertyFlags::IS_STATIC
    } else {
        PropertyFlags::NONE
    };
    let mut close_bracket_loc = Loc::default();
    let key = if let Some((name, loc)) = preconsumed_static {
        class_property_name(core, loc, name)
    } else {
        match lexer.token {
            Token::PrivateIdentifier => {
                let reference = core.store_name_in_ref(lexer.identifier.clone());
                let key = Expr::new(
                    lexer.loc(),
                    ExprData::PrivateIdentifier(PrivateIdentifierExpr { reference }),
                );
                lexer.next();
                key
            }
            Token::OpenBracket => {
                flags |= PropertyFlags::IS_COMPUTED;
                lexer.next();
                let key = parse_expression(core, lexer, Precedence::Comma, true);
                close_bracket_loc = lexer.loc();
                lexer.expect(Token::CloseBracket);
                key
            }
            Token::NumericLiteral => parse_numeric_literal(core, lexer),
            Token::BigIntegerLiteral => {
                let key = parse_big_int_or_string_if_unsupported(core, lexer);
                lexer.next();
                key
            }
            Token::StringLiteral => parse_string_literal(core, lexer),
            _ => {
                if !lexer.is_identifier_or_keyword() {
                    lexer.expected(Token::Identifier);
                }
                let name = lexer.identifier.clone();
                lexer.next();
                class_property_name(core, key_loc, name)
            }
        }
    };

    if lexer.token == Token::OpenParen {
        let is_constructor = !is_static && key_is_named(&key, "constructor");
        let function = parse_function_tail(
            core,
            lexer,
            None,
            FnOrArrowDataParse {
                yield_policy: if is_generator {
                    AwaitOrYield::AllowExpression
                } else {
                    AwaitOrYield::AllowIdentifier
                },
                allow_super_call: class_has_extends && is_constructor,
                allow_super_property: true,
                ..FnOrArrowDataParse::default()
            },
        );
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
            kind: PropertyKind::Method,
            flags,
            ..Property::default()
        };
    }
    if is_generator {
        lexer.expected(Token::OpenParen);
    }

    let initializer_or_nil = if lexer.token == Token::Equals {
        lexer.next();
        let old_context = core.fn_or_arrow_data_parse;
        core.fn_or_arrow_data_parse.is_this_disallowed = false;
        core.fn_or_arrow_data_parse.allow_super_property = true;
        let value = parse_expression(core, lexer, Precedence::Comma, true);
        core.fn_or_arrow_data_parse = old_context;
        value
    } else {
        Expr::default()
    };
    lexer.expect_or_insert_semicolon();
    Property {
        key,
        initializer_or_nil,
        loc: start_loc,
        close_bracket_loc,
        kind: PropertyKind::Field,
        flags,
        ..Property::default()
    }
}

fn class_property_name(core: &mut ParserCore, loc: Loc, name: MaybeSubstring) -> Expr {
    let text =
        String::from_utf8(name.string.clone()).expect("class property names must be valid UTF-8");
    if core.is_mangled_prop(&text) {
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
                value: string_to_utf16(text.as_bytes()),
                ..StringExpr::default()
            }),
        )
    }
}

fn key_is_named(key: &Expr, expected: &str) -> bool {
    matches!(
        key.data.as_deref(),
        Some(ExprData::String(string))
            if crate::internal::helpers::utf16_to_string(&string.value) == expected.as_bytes()
    )
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::parse_class_prefix;
    use crate::internal::{
        config::TsOptions,
        js_ast::{ExprData, PropertyFlags, PropertyKind},
        js_lexer::{Lexer, Token},
        js_parser::Options,
        logger::{DeferLogKind, Log, Source},
    };

    #[test]
    fn parses_class_name_extends_methods_fields_static_and_private_keys() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                &b"class Child extends Base { field = 1; static count = 2; #private; method() { return this.field } static { cleanup() } }"[..],
            ),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let expr = parse_class_prefix(&mut core, &mut lexer).expect("class");
        let Some(ExprData::Class(class)) = expr.data.as_deref() else {
            panic!("expected class");
        };
        assert!(class.class.name.is_some());
        assert!(class.class.extends_or_nil.data.is_some());
        assert_eq!(class.class.properties.len(), 5);
        assert!(
            class.class.properties[1]
                .flags
                .contains(PropertyFlags::IS_STATIC)
        );
        assert_eq!(class.class.properties[3].kind, PropertyKind::Method);
        assert_eq!(
            class.class.properties[4].kind,
            PropertyKind::ClassStaticBlock
        );
        assert_eq!(lexer.token, Token::EndOfFile);
    }
}
