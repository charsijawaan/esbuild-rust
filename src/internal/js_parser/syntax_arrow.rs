#![allow(dead_code)]

use crate::internal::{
    js_ast::{
        Arg, ArrayBinding, ArrayBindingPattern, ArrowExpr, Binding, BindingData, Expr, ExprData,
        FunctionBody, IdentifierBinding, ObjectBindingPattern, OpCode, Precedence, PropertyBinding,
        PropertyFlags, PropertyKind, ReturnStmt, Stmt, StmtData,
    },
    js_lexer::{Lexer, Token},
};

use super::{
    parser_core::ParserCore,
    parser_types::{AwaitOrYield, FnOrArrowDataParse},
    syntax_expression::parse_expression,
    syntax_statement::parse_block_with_scope,
};

pub(crate) fn parse_identifier_or_arrow_prefix(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    minimum_precedence: Precedence,
) -> Option<Expr> {
    if lexer.token != Token::Identifier {
        return None;
    }
    let loc = lexer.loc();
    let reference = core.store_name_in_ref(lexer.identifier.clone());
    lexer.next();

    if lexer.token == Token::EqualsGreaterThan && minimum_precedence <= Precedence::Assign {
        let arg = Arg {
            binding: Binding {
                loc,
                data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                    reference,
                }))),
            },
            ..Arg::default()
        };
        return Some(parse_arrow_body(
            core,
            lexer,
            loc,
            vec![arg],
            false,
            false,
            false,
        ));
    }

    Some(Expr::new(
        loc,
        ExprData::Identifier(crate::internal::js_ast::IdentifierExpr {
            reference,
            ..crate::internal::js_ast::IdentifierExpr::default()
        }),
    ))
}

pub(crate) fn parse_empty_parenthesized_arrow(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    loc: crate::internal::logger::Loc,
) -> Option<Expr> {
    lexer.expect(Token::CloseParen);
    if core.options.ts.parse {
        super::syntax_typescript::skip_type_annotation(lexer, &[Token::EqualsGreaterThan]);
    }
    if lexer.token != Token::EqualsGreaterThan {
        return None;
    }
    Some(parse_arrow_body(
        core,
        lexer,
        loc,
        Vec::new(),
        false,
        false,
        false,
    ))
}

pub(crate) fn parse_arrow_after_parenthesized_expression(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    loc: crate::internal::logger::Loc,
    expression: Expr,
) -> Option<Expr> {
    if lexer.token != Token::EqualsGreaterThan {
        return None;
    }
    let mut args = Vec::new();
    let mut has_rest_arg = false;
    if !convert_expression_to_args(expression, &mut args, &mut has_rest_arg) {
        core.add_error_range(lexer.range(), "Invalid arrow function parameter list");
    }
    Some(parse_arrow_body(
        core,
        lexer,
        loc,
        args,
        false,
        false,
        has_rest_arg,
    ))
}

fn convert_expression_to_args(
    expression: Expr,
    args: &mut Vec<Arg>,
    has_rest_arg: &mut bool,
) -> bool {
    let loc = expression.loc;
    let Some(data) = expression.data else {
        return false;
    };
    match *data {
        ExprData::Binary(binary) if binary.op == OpCode::BinaryComma => {
            convert_expression_to_args(binary.left, args, has_rest_arg)
                && convert_expression_to_args(binary.right, args, has_rest_arg)
        }
        ExprData::Binary(binary) if binary.op == OpCode::BinaryAssign => {
            let Some(binding) = expression_to_binding(binary.left) else {
                return false;
            };
            args.push(Arg {
                binding,
                default_or_nil: binary.right,
                ..Arg::default()
            });
            true
        }
        ExprData::Identifier(identifier) => {
            args.push(Arg {
                binding: Binding {
                    loc,
                    data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                        reference: identifier.reference,
                    }))),
                },
                ..Arg::default()
            });
            true
        }
        data @ (ExprData::Array(_) | ExprData::Object(_)) => {
            let Some(binding) = expression_to_binding(Expr::new(loc, data)) else {
                return false;
            };
            args.push(Arg {
                binding,
                ..Arg::default()
            });
            true
        }
        ExprData::Spread(spread) => {
            let Some(binding) = expression_to_binding(spread.value) else {
                return false;
            };
            args.push(Arg {
                binding,
                ..Arg::default()
            });
            *has_rest_arg = true;
            true
        }
        _ => false,
    }
}

fn expression_to_binding(expression: Expr) -> Option<Binding> {
    let loc = expression.loc;
    match *expression.data? {
        ExprData::Identifier(identifier) => Some(Binding {
            loc,
            data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                reference: identifier.reference,
            }))),
        }),
        ExprData::Array(array) => {
            let mut items = Vec::with_capacity(array.items.len());
            let mut has_spread = false;
            for item in array.items {
                let item_loc = item.loc;
                let (binding, default_value_or_nil) = match item.data.as_deref() {
                    Some(ExprData::Missing) => (
                        Binding {
                            loc: item_loc,
                            data: Some(Box::new(BindingData::Missing)),
                        },
                        Expr::default(),
                    ),
                    Some(ExprData::Spread(_)) => {
                        let ExprData::Spread(spread) = *item.data.expect("spread item has data")
                        else {
                            unreachable!()
                        };
                        has_spread = true;
                        (expression_to_binding(spread.value)?, Expr::default())
                    }
                    Some(ExprData::Binary(binary)) if binary.op == OpCode::BinaryAssign => {
                        let ExprData::Binary(binary) =
                            *item.data.expect("assignment item has data")
                        else {
                            unreachable!()
                        };
                        (expression_to_binding(binary.left)?, binary.right)
                    }
                    _ => (expression_to_binding(item)?, Expr::default()),
                };
                items.push(ArrayBinding {
                    binding,
                    default_value_or_nil,
                    loc: item_loc,
                });
            }
            Some(Binding {
                loc,
                data: Some(Box::new(BindingData::Array(ArrayBindingPattern {
                    items,
                    close_bracket_loc: array.close_bracket_loc,
                    has_spread,
                    is_single_line: array.is_single_line,
                }))),
            })
        }
        ExprData::Object(object) => {
            let mut properties = Vec::with_capacity(object.properties.len());
            for property in object.properties {
                if property.kind == PropertyKind::Spread {
                    properties.push(PropertyBinding {
                        value: expression_to_binding(property.value_or_nil)?,
                        loc: property.loc,
                        is_spread: true,
                        ..PropertyBinding::default()
                    });
                    continue;
                }
                if property.kind != PropertyKind::Field {
                    return None;
                }
                properties.push(PropertyBinding {
                    key: property.key,
                    value: expression_to_binding(property.value_or_nil)?,
                    default_value_or_nil: property.initializer_or_nil,
                    loc: property.loc,
                    close_bracket_loc: property.close_bracket_loc,
                    is_computed: property.flags.contains(PropertyFlags::IS_COMPUTED),
                    prefer_quoted_key: property.flags.contains(PropertyFlags::PREFER_QUOTED_KEY),
                    ..PropertyBinding::default()
                });
            }
            Some(Binding {
                loc,
                data: Some(Box::new(BindingData::Object(ObjectBindingPattern {
                    properties,
                    close_brace_loc: object.close_brace_loc,
                    is_single_line: object.is_single_line,
                }))),
            })
        }
        _ => None,
    }
}

pub(crate) fn parse_arrow_body(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    loc: crate::internal::logger::Loc,
    args: Vec<Arg>,
    is_async: bool,
    is_parenthesized: bool,
    has_rest_arg: bool,
) -> Expr {
    if lexer.has_newline_before {
        core.add_error_range(lexer.range(), "Unexpected newline before \"=>\"");
    }
    let arrow_loc = lexer.loc();
    lexer.expect(Token::EqualsGreaterThan);
    core.push_scope_for_parse_pass(crate::internal::js_ast::ScopeKind::FunctionArgs, arrow_loc);
    let mut args = args;
    for argument in &mut args {
        core.declare_binding(
            crate::internal::ast::SymbolKind::Hoisted,
            &mut argument.binding,
        );
    }

    let old_context = core.fn_or_arrow_data_parse;
    core.fn_or_arrow_data_parse = FnOrArrowDataParse {
        await_policy: if is_async {
            AwaitOrYield::AllowExpression
        } else {
            AwaitOrYield::AllowIdentifier
        },
        is_this_disallowed: old_context.is_this_disallowed,
        allow_super_call: old_context.allow_super_call,
        allow_super_property: old_context.allow_super_property,
        ..FnOrArrowDataParse::default()
    };

    let (body, prefer_expr) = if lexer.token == Token::OpenBrace {
        let (body_loc, block) = parse_block_with_scope(
            core,
            lexer,
            crate::internal::js_ast::ScopeKind::FunctionBody,
        );
        (
            FunctionBody {
                block,
                loc: body_loc,
            },
            false,
        )
    } else {
        let body_loc = lexer.loc();
        core.push_scope_for_parse_pass(crate::internal::js_ast::ScopeKind::FunctionBody, body_loc);
        let value = parse_expression(core, lexer, Precedence::Comma, true);
        core.pop_scope();
        (
            FunctionBody {
                block: crate::internal::js_ast::BlockStmt {
                    statements: vec![Stmt::new(
                        value.loc,
                        StmtData::Return(ReturnStmt {
                            value_or_nil: value,
                        }),
                    )],
                    ..crate::internal::js_ast::BlockStmt::default()
                },
                loc: body_loc,
            },
            true,
        )
    };
    core.fn_or_arrow_data_parse = old_context;
    core.pop_scope();

    Expr::new(
        loc,
        ExprData::Arrow(ArrowExpr {
            args,
            body,
            is_async,
            has_rest_arg,
            prefer_expr,
            is_parenthesized,
            ..ArrowExpr::default()
        }),
    )
}

pub(crate) fn is_async_arrow_call(core: &ParserCore, expression: &Expr) -> bool {
    let Some(ExprData::Call(call)) = expression.data.as_deref() else {
        return false;
    };
    let Some(ExprData::Identifier(identifier)) = call.target.data.as_deref() else {
        return false;
    };
    if core.load_name_from_ref(identifier.reference) != b"async" {
        return false;
    }

    let start = usize::try_from(call.target.loc.start + 5).unwrap_or_default();
    let suffix = core.source.contents.get(start..).unwrap_or_default();
    let before_open_paren = suffix
        .split(|byte| *byte == b'(')
        .next()
        .unwrap_or_default();
    !before_open_paren
        .iter()
        .any(|byte| matches!(byte, b'\n' | b'\r'))
}

pub(crate) fn parse_async_arrow_from_call(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    expression: Expr,
) -> Expr {
    let loc = expression.loc;
    let ExprData::Call(call) = *expression
        .data
        .expect("async arrow candidate must have expression data")
    else {
        unreachable!("async arrow candidate must be a call");
    };
    let mut args = Vec::new();
    let mut has_rest_arg = false;
    let mut valid = true;
    for argument in call.args {
        valid &= convert_expression_to_args(argument, &mut args, &mut has_rest_arg);
    }
    if !valid {
        core.add_error_range(lexer.range(), "Invalid arrow function parameter list");
    }
    parse_arrow_body(core, lexer, loc, args, true, false, has_rest_arg)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use crate::internal::{
        config::TsOptions,
        js_ast::{BindingData, ExprData, Precedence},
        js_lexer::{Lexer, Token},
        js_parser::{Options, syntax_expression::parse_expression},
        logger::{DeferLogKind, Log, Source},
    };

    #[test]
    fn parses_identifier_parenthesized_default_and_empty_arrows() {
        for (text, arg_count) in [
            ("value => value + 1", 1),
            ("(a, b = 2) => a + b", 2),
            ("() => 1", 0),
        ] {
            let log = Log::new_defer(DeferLogKind::All, HashMap::new());
            let source = Source {
                contents: Arc::from(text.as_bytes()),
                ..Source::default()
            };
            let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
            let mut core = super::ParserCore::new(source, Options::default());
            let expression = parse_expression(&mut core, &mut lexer, Precedence::Lowest, true);
            assert!(matches!(
                expression.data.as_deref(),
                Some(ExprData::Arrow(arrow)) if arrow.args.len() == arg_count
            ));
            assert_eq!(lexer.token, Token::EndOfFile);
        }
    }

    #[test]
    fn parses_async_identifier_and_parenthesized_arrows() {
        for (text, arg_count) in [
            ("async value => await value", 1),
            ("async (a, b = 2) => await a", 2),
        ] {
            let log = Log::new_defer(DeferLogKind::All, HashMap::new());
            let source = Source {
                contents: Arc::from(text.as_bytes()),
                ..Source::default()
            };
            let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
            let mut core = super::ParserCore::new(source, Options::default());
            let expression = parse_expression(&mut core, &mut lexer, Precedence::Lowest, true);
            assert!(matches!(
                expression.data.as_deref(),
                Some(ExprData::Arrow(arrow))
                    if arrow.is_async && arrow.args.len() == arg_count
            ));
            assert_eq!(lexer.token, Token::EndOfFile);
        }
    }

    #[test]
    fn parses_sync_and_async_rest_arguments() {
        for text in [
            "(first, ...rest) => rest",
            "async (...values) => await values",
        ] {
            let log = Log::new_defer(DeferLogKind::All, HashMap::new());
            let source = Source {
                contents: Arc::from(text.as_bytes()),
                ..Source::default()
            };
            let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
            let mut core = super::ParserCore::new(source, Options::default());
            let expression = parse_expression(&mut core, &mut lexer, Precedence::Lowest, true);
            assert!(matches!(
                expression.data.as_deref(),
                Some(ExprData::Arrow(arrow)) if arrow.has_rest_arg
            ));
            assert_eq!(lexer.token, Token::EndOfFile);
        }
    }

    #[test]
    fn converts_array_and_object_expressions_to_arrow_bindings() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"([first, ...tail], {value}) => value"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let expression = parse_expression(&mut core, &mut lexer, Precedence::Lowest, true);
        let Some(ExprData::Arrow(arrow)) = expression.data.as_deref() else {
            panic!("expected arrow");
        };
        assert!(matches!(
            arrow.args[0].binding.data.as_deref(),
            Some(BindingData::Array(_))
        ));
        assert!(matches!(
            arrow.args[1].binding.data.as_deref(),
            Some(BindingData::Object(_))
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }
}
