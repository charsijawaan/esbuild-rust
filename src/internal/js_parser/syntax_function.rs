#![allow(dead_code)]

use crate::internal::{
    ast::{INVALID_REF, LocRef},
    js_ast::{
        Arg, Binding, BindingData, Expr, ExprData, Function, FunctionExpr, IdentifierBinding,
        Precedence,
    },
    js_lexer::{Lexer, Token},
};

use super::{
    parser_core::ParserCore,
    parser_types::{AwaitOrYield, FnOrArrowDataParse},
    syntax_arrow::parse_arrow_body,
    syntax_binding::parse_binding,
    syntax_expression::parse_expression,
    syntax_statement::parse_block_with_scope,
};

pub(crate) fn parse_function_prefix(core: &mut ParserCore, lexer: &mut Lexer) -> Option<Expr> {
    if lexer.token != Token::Function {
        return None;
    }

    let loc = lexer.loc();
    Some(parse_function_after_keyword(core, lexer, loc, false))
}

pub(crate) fn parse_async_prefix(core: &mut ParserCore, lexer: &mut Lexer) -> Option<Expr> {
    if lexer.token != Token::Identifier || lexer.raw() != b"async" {
        return None;
    }

    let loc = lexer.loc();
    let reference = core.store_name_in_ref(lexer.identifier.clone());
    lexer.next();
    if !lexer.has_newline_before && lexer.token == Token::Function {
        return Some(parse_function_after_keyword(core, lexer, loc, true));
    }
    if !lexer.has_newline_before && lexer.token == Token::Identifier {
        let arg_loc = lexer.loc();
        let arg_reference = core.store_name_in_ref(lexer.identifier.clone());
        lexer.next();
        if lexer.token == Token::EqualsGreaterThan {
            return Some(parse_arrow_body(
                core,
                lexer,
                loc,
                vec![Arg {
                    binding: Binding {
                        loc: arg_loc,
                        data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                            reference: arg_reference,
                        }))),
                    },
                    ..Arg::default()
                }],
                true,
                false,
                false,
            ));
        }
    }

    Some(Expr::new(
        loc,
        ExprData::Identifier(crate::internal::js_ast::IdentifierExpr {
            reference,
            ..crate::internal::js_ast::IdentifierExpr::default()
        }),
    ))
}

fn parse_function_after_keyword(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    loc: crate::internal::logger::Loc,
    is_async: bool,
) -> Expr {
    lexer.expect(Token::Function);
    let is_generator = lexer.token == Token::Asterisk;
    if is_generator {
        lexer.next();
    }

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

    let function = parse_function_tail(
        core,
        lexer,
        name,
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
            ..FnOrArrowDataParse::default()
        },
    );

    Expr::new(
        loc,
        ExprData::Function(FunctionExpr {
            function,
            ..FunctionExpr::default()
        }),
    )
}

pub(crate) fn parse_function_tail(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    name: Option<LocRef>,
    body_context: FnOrArrowDataParse,
) -> Function {
    let is_async = body_context.await_policy == AwaitOrYield::AllowExpression;
    let is_generator = body_context.yield_policy == AwaitOrYield::AllowExpression;
    let open_paren_loc = lexer.loc();
    core.push_scope_for_parse_pass(
        crate::internal::js_ast::ScopeKind::FunctionArgs,
        open_paren_loc,
    );
    lexer.expect(Token::OpenParen);

    let old_context = core.fn_or_arrow_data_parse;
    core.fn_or_arrow_data_parse = FnOrArrowDataParse {
        await_policy: if is_async {
            AwaitOrYield::ForbidAll
        } else {
            AwaitOrYield::AllowIdentifier
        },
        yield_policy: if is_generator {
            AwaitOrYield::ForbidAll
        } else {
            AwaitOrYield::AllowIdentifier
        },
        allow_super_call: body_context.allow_super_call,
        allow_super_property: body_context.allow_super_property,
        ..FnOrArrowDataParse::default()
    };

    let mut args = Vec::new();
    let mut has_rest_arg = false;
    while lexer.token != Token::CloseParen {
        if lexer.token == Token::DotDotDot {
            lexer.next();
            has_rest_arg = true;
        }

        let binding = parse_binding(core, lexer);

        let default_or_nil = if !has_rest_arg && lexer.token == Token::Equals {
            lexer.next();
            parse_expression(core, lexer, Precedence::Comma, true)
        } else {
            Expr::default()
        };
        args.push(Arg {
            binding,
            default_or_nil,
            ..Arg::default()
        });

        if lexer.token != Token::Comma {
            break;
        }
        if has_rest_arg {
            lexer.expected(Token::CloseParen);
        }
        lexer.next();
    }
    lexer.expect(Token::CloseParen);

    core.fn_or_arrow_data_parse = body_context;
    let (body_loc, block) = parse_block_with_scope(
        core,
        lexer,
        crate::internal::js_ast::ScopeKind::FunctionBody,
    );
    core.pop_scope();
    core.fn_or_arrow_data_parse = old_context;

    Function {
        name,
        args,
        body: crate::internal::js_ast::FunctionBody {
            block,
            loc: body_loc,
        },
        arguments_ref: INVALID_REF,
        open_paren_loc,
        is_async,
        is_generator,
        has_rest_arg,
        ..Function::default()
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::{parse_async_prefix, parse_function_prefix};
    use crate::internal::{
        config::TsOptions,
        js_ast::{ExprData, StmtData},
        js_lexer::{Lexer, Token},
        js_parser::Options,
        logger::{DeferLogKind, Log, Source},
    };

    #[test]
    fn parses_named_generator_arguments_defaults_rest_and_body() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"function* name(a = 1, ...rest) { yield a; }"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let expr = parse_function_prefix(&mut core, &mut lexer).expect("function");
        let Some(ExprData::Function(function)) = expr.data.as_deref() else {
            panic!("expected function");
        };
        assert!(function.function.is_generator);
        assert!(function.function.has_rest_arg);
        assert_eq!(function.function.args.len(), 2);
        assert!(matches!(
            function.function.body.block.statements[0].data.as_deref(),
            Some(StmtData::Expr(_))
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn parses_async_function_and_async_generator_prefixes() {
        for (text, generator) in [
            (&b"async function name() { await work }"[..], false),
            (&b"async function* name() { yield await work }"[..], true),
        ] {
            let log = Log::new_defer(DeferLogKind::All, HashMap::new());
            let source = Source {
                contents: Arc::from(text),
                ..Source::default()
            };
            let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
            let mut core = super::ParserCore::new(source, Options::default());
            let expr = parse_async_prefix(&mut core, &mut lexer).expect("async function");
            assert!(matches!(
                expr.data.as_deref(),
                Some(ExprData::Function(function))
                    if function.function.is_async
                        && function.function.is_generator == generator
            ));
            assert_eq!(lexer.token, Token::EndOfFile);
        }
    }
}
