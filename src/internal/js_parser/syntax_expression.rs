#![allow(dead_code)]

use crate::internal::{
    compat::JsFeature,
    js_ast::{BinaryExpr, Expr, ExprData, IfExpr, OpCode, Precedence, SpreadExpr},
    js_lexer::{CommentBefore, Lexer, Token},
};

use super::{
    parser_core::ParserCore,
    syntax_arrow::{
        expression_can_be_arrow_args, is_async_arrow_call,
        parse_arrow_after_parenthesized_expression, parse_async_arrow_from_call,
        parse_empty_parenthesized_arrow, parse_identifier_or_arrow_prefix,
    },
    syntax_class::parse_class_prefix,
    syntax_function::{parse_async_prefix, parse_function_prefix},
    syntax_import::parse_import_prefix,
    syntax_jsx::parse_jsx_element_prefix,
    syntax_literals::{
        parse_array_prefix, parse_numeric_literal, parse_regular_expression_literal,
        parse_simple_prefix, parse_string_literal, parse_unary_prefix,
        parse_untagged_template_prefix,
    },
    syntax_new::parse_new_prefix,
    syntax_object::parse_object_literal_prefix,
    syntax_private::parse_private_brand_check_prefix,
    syntax_suffix::{binary_operator, parse_high_precedence_suffix_chain},
    syntax_super::parse_super_prefix,
    syntax_yield_await::parse_await_or_yield_prefix,
};

fn mark_unlowered_exponentiation_assignment(core: &mut ParserCore, lexer: &Lexer, op: OpCode) {
    if op == OpCode::BinaryPowerAssign {
        core.mark_syntax_feature(JsFeature::EXPONENT_OPERATOR, lexer.range());
    }
}

pub(crate) fn parse_expression(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    minimum_precedence: Precedence,
    allow_in: bool,
) -> Expr {
    let comment_flags = lexer.has_comment_before;
    let left = parse_prefix(core, lexer, minimum_precedence, allow_in);
    parse_expression_suffix_with_flags(
        core,
        lexer,
        left,
        minimum_precedence,
        allow_in,
        comment_flags,
    )
}

pub(crate) fn parse_expression_suffix_with_flags(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    mut left: Expr,
    minimum_precedence: Precedence,
    allow_in: bool,
    comment_flags: CommentBefore,
) -> Expr {
    if !core.options.ignore_dce_annotations {
        if comment_flags.contains(CommentBefore::NO_SIDE_EFFECTS) {
            match left.data.as_deref_mut() {
                Some(ExprData::Arrow(arrow)) => arrow.has_no_side_effects_comment = true,
                Some(ExprData::Function(function)) => {
                    function.function.has_no_side_effects_comment = true;
                }
                _ => {}
            }
        }
        if comment_flags.contains(CommentBefore::PURE) && minimum_precedence < Precedence::Call {
            left = parse_high_precedence_suffix_chain(
                core,
                lexer,
                left,
                Precedence::New,
                false,
                |core, lexer, precedence| parse_expression(core, lexer, precedence, true),
            );
            match left.data.as_deref_mut() {
                Some(ExprData::Call(call)) => call.can_be_unwrapped_if_unused = true,
                Some(ExprData::New(new)) => new.can_be_unwrapped_if_unused = true,
                _ => {}
            }
        }
    }
    parse_expression_suffix(core, lexer, left, minimum_precedence, allow_in)
}

pub(crate) fn parse_expression_suffix(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    mut left: Expr,
    minimum_precedence: Precedence,
    allow_in: bool,
) -> Expr {
    left = parse_high_precedence_suffix_chain(
        core,
        lexer,
        left,
        minimum_precedence,
        false,
        |core, lexer, precedence| parse_expression(core, lexer, precedence, true),
    );
    if lexer.token == Token::EqualsGreaterThan && is_async_arrow_call(core, &left) {
        return parse_async_arrow_from_call(core, lexer, left);
    }

    let mut previous_operator = None;
    loop {
        if core.options.ts.parse
            && lexer.token == Token::LessThan
            && super::syntax_typescript::try_skip_type_arguments_in_expression(lexer)
        {
            left = parse_high_precedence_suffix_chain(
                core,
                lexer,
                left,
                minimum_precedence,
                false,
                |core, lexer, precedence| parse_expression(core, lexer, precedence, true),
            );
            continue;
        }
        if core.options.ts.parse
            && (lexer.is_contextual_keyword(b"as") || lexer.is_contextual_keyword(b"satisfies"))
        {
            lexer.next();
            super::syntax_typescript::skip_type_assertion(lexer);
            continue;
        }
        if lexer.token == Token::Question && Precedence::Conditional > minimum_precedence {
            lexer.next();
            if core.options.ts.parse && lexer.token == Token::Colon {
                return left;
            }
            let yes = parse_expression(core, lexer, Precedence::Comma, true);
            lexer.expect(Token::Colon);
            let no = parse_expression(core, lexer, Precedence::Comma, allow_in);
            left = Expr::new(
                left.loc,
                ExprData::If(IfExpr {
                    test: left,
                    yes,
                    no,
                }),
            );
            continue;
        }

        let Some(operator) = binary_operator(lexer.token) else {
            return left;
        };
        if operator.precedence <= minimum_precedence || (lexer.token == Token::In && !allow_in) {
            return left;
        }
        let mixes_nullish = (minimum_precedence == Precedence::NullishCoalescing
            && matches!(
                operator.op,
                crate::internal::js_ast::OpCode::BinaryLogicalOr
                    | crate::internal::js_ast::OpCode::BinaryLogicalAnd
            ))
            || (operator.op == crate::internal::js_ast::OpCode::BinaryNullishCoalescing
                && matches!(
                    previous_operator,
                    Some(
                        crate::internal::js_ast::OpCode::BinaryLogicalOr
                            | crate::internal::js_ast::OpCode::BinaryLogicalAnd
                    )
                ));
        if mixes_nullish {
            core.add_error_range(
                lexer.range(),
                "Cannot mix \"??\" with \"||\" or \"&&\" without parentheses",
            );
        }
        mark_unlowered_exponentiation_assignment(core, lexer, operator.op);
        lexer.next();
        let right_minimum = if operator.is_right_associative {
            match operator.precedence {
                Precedence::Assign => Precedence::Yield,
                Precedence::Exponentiation => Precedence::Multiply,
                _ => operator.precedence,
            }
        } else {
            operator.precedence
        };
        let right = parse_expression(core, lexer, right_minimum, allow_in);
        left = Expr::new(
            left.loc,
            ExprData::Binary(BinaryExpr {
                left,
                right,
                op: operator.op,
            }),
        );
        previous_operator = Some(operator.op);
    }
}

#[allow(clippy::too_many_lines)]
fn parse_prefix(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    minimum_precedence: Precedence,
    allow_in: bool,
) -> Expr {
    if let Some(expr) = parse_async_prefix(core, lexer) {
        return expr;
    }
    if let Some(expr) = parse_super_prefix(core, lexer, minimum_precedence) {
        return expr;
    }
    if let Some(expr) = parse_private_brand_check_prefix(core, lexer, minimum_precedence, allow_in)
    {
        return expr;
    }
    if let Some(expr) = parse_await_or_yield_prefix(
        core,
        lexer,
        minimum_precedence,
        |core, lexer, precedence| parse_expression(core, lexer, precedence, allow_in),
    ) {
        return expr;
    }
    if let Some(expr) = parse_identifier_or_arrow_prefix(core, lexer, minimum_precedence) {
        return expr;
    }
    if let Some(expr) = parse_jsx_element_prefix(core, lexer) {
        return expr;
    }
    if let Some(expr) = parse_simple_prefix(core, lexer) {
        return expr;
    }
    if lexer.token == Token::At {
        return parse_class_prefix(core, lexer).expect("decorator must be followed by a class");
    }
    match lexer.token {
        Token::LessThan if core.options.ts.parse => {
            super::syntax_typescript::skip_type_parameters(lexer);
            parse_expression(core, lexer, Precedence::Prefix, allow_in)
        }
        Token::Class => parse_class_prefix(core, lexer).expect("class token was checked"),
        Token::Function => parse_function_prefix(core, lexer).expect("function token was checked"),
        Token::New => parse_new_prefix(core, lexer, parse_new_target, |core, lexer| {
            parse_expression(core, lexer, Precedence::Comma, true)
        })
        .expect("new token was checked"),
        Token::Import => parse_import_prefix(core, lexer, |core, lexer| {
            parse_expression(core, lexer, Precedence::Comma, true)
        })
        .expect("import token was checked"),
        Token::StringLiteral | Token::NoSubstitutionTemplateLiteral => {
            parse_string_literal(core, lexer)
        }
        Token::TemplateHead => parse_untagged_template_prefix(lexer, |lexer| {
            parse_expression(core, lexer, Precedence::Lowest, true)
        })
        .expect("template token was checked"),
        Token::NumericLiteral => parse_numeric_literal(core, lexer),
        Token::BigIntegerLiteral => {
            let loc = lexer.loc();
            let value = std::str::from_utf8(&lexer.identifier.string)
                .expect("big integer tokens must be valid ASCII")
                .to_string();
            lexer.next();
            Expr::new(loc, ExprData::BigInt(value))
        }
        Token::Slash | Token::SlashEquals => parse_regular_expression_literal(lexer),
        Token::OpenBracket => parse_array_prefix(lexer, |lexer| {
            parse_expression(core, lexer, Precedence::Comma, true)
        })
        .expect("array token was checked"),
        Token::OpenBrace => parse_object_literal_prefix(core, lexer, |core, lexer, precedence| {
            parse_expression(core, lexer, precedence, true)
        })
        .expect("object token was checked"),
        Token::OpenParen => {
            let paren_loc = lexer.loc();
            // This may be an arrow function, so create the argument scope before
            // parsing the parameters. Default values can contain nested scopes,
            // and those scopes must be children of the arrow's argument scope.
            // This mirrors esbuild's speculative scope handling in parseParenExpr.
            let scope_index = core.push_scope_for_parse_pass(
                crate::internal::js_ast::ScopeKind::FunctionArgs,
                paren_loc,
            );
            lexer.next();
            if lexer.token == Token::CloseParen {
                let result = parse_empty_parenthesized_arrow(core, lexer, paren_loc)
                    .unwrap_or_else(|| lexer.unexpected());
                core.pop_scope();
                return result;
            }
            let mut expr = parse_parenthesized_item(core, lexer);
            if core.options.ts.parse {
                expr = parse_type_script_parameter_suffix(core, lexer, expr);
            }
            let mut has_trailing_comma = false;
            let mut has_rest = matches!(expr.data.as_deref(), Some(ExprData::Spread(_)));
            while lexer.token == Token::Comma {
                let comma_range = lexer.range();
                lexer.next();
                if lexer.token == Token::CloseParen {
                    has_trailing_comma = true;
                    if has_rest {
                        core.add_error_range(comma_range, "Unexpected \",\" after rest pattern");
                    }
                    break;
                }
                if has_rest {
                    core.add_error_range(comma_range, "Unexpected \",\" after rest pattern");
                }
                let mut right = parse_parenthesized_item(core, lexer);
                if core.options.ts.parse {
                    right = parse_type_script_parameter_suffix(core, lexer, right);
                }
                has_rest |= matches!(right.data.as_deref(), Some(ExprData::Spread(_)));
                expr = Expr::new(
                    expr.loc,
                    ExprData::Binary(BinaryExpr {
                        left: expr,
                        right,
                        op: crate::internal::js_ast::OpCode::BinaryComma,
                    }),
                );
            }
            lexer.expect(Token::CloseParen);
            if core.options.ts.parse && expression_can_be_arrow_args(core, &expr) {
                super::syntax_typescript::try_skip_arrow_return_type(lexer);
            }
            if lexer.token == Token::EqualsGreaterThan {
                let result =
                    parse_arrow_after_parenthesized_expression(core, lexer, paren_loc, expr)
                        .expect("arrow token was checked");
                core.pop_scope();
                result
            } else {
                // It was an ordinary parenthesized expression. Remove the
                // speculative argument scope but retain any nested child scopes.
                core.pop_and_flatten_scope(scope_index);
                if has_trailing_comma || has_rest {
                    lexer.unexpected();
                }
                match expr.data.as_deref_mut() {
                    Some(ExprData::Array(array)) => array.is_parenthesized = true,
                    Some(ExprData::Object(object)) => object.is_parenthesized = true,
                    Some(ExprData::Function(function)) => function.is_parenthesized = true,
                    Some(ExprData::Arrow(arrow)) => arrow.is_parenthesized = true,
                    _ => {}
                }
                expr
            }
        }
        _ => parse_unary_prefix(lexer, |lexer| {
            parse_expression(core, lexer, Precedence::Prefix, allow_in)
        })
        .unwrap_or_else(|| lexer.unexpected()),
    }
}

fn parse_type_script_parameter_suffix(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    mut expression: Expr,
) -> Expr {
    if lexer.token == Token::Question {
        lexer.next();
    }
    super::syntax_typescript::skip_type_annotation(
        lexer,
        &[
            Token::Equals,
            Token::Comma,
            Token::CloseParen,
            Token::EqualsGreaterThan,
        ],
    );
    if lexer.token == Token::Equals {
        lexer.next();
        let right = parse_expression(core, lexer, Precedence::Comma, true);
        expression = Expr::new(
            expression.loc,
            ExprData::Binary(BinaryExpr {
                left: expression,
                right,
                op: crate::internal::js_ast::OpCode::BinaryAssign,
            }),
        );
    }
    expression
}

fn parse_new_target(core: &mut ParserCore, lexer: &mut Lexer) -> Expr {
    let mut target = parse_prefix(core, lexer, Precedence::Member, true);
    target = parse_high_precedence_suffix_chain(
        core,
        lexer,
        target,
        Precedence::Member,
        true,
        |core, lexer, precedence| parse_expression(core, lexer, precedence, true),
    );
    target
}

fn parse_parenthesized_item(core: &mut ParserCore, lexer: &mut Lexer) -> Expr {
    if lexer.token == Token::DotDotDot {
        let loc = lexer.loc();
        core.mark_syntax_feature(JsFeature::REST_ARGUMENT, lexer.range());
        lexer.next();
        let value = parse_expression(core, lexer, Precedence::Comma, true);
        Expr::new(loc, ExprData::Spread(SpreadExpr { value }))
    } else {
        parse_expression(core, lexer, Precedence::Comma, true)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::parse_expression;
    use crate::internal::{
        config::TsOptions,
        js_ast::{ExprData, OpCode, Precedence},
        js_lexer::{Lexer, Token},
        js_parser::{Options, parser_types::AwaitOrYield},
        logger::{DeferLogKind, Log, Source},
    };

    #[test]
    fn preserves_unsupported_bigint_expressions_for_printer_lowering() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"0xCAFE_BABEn"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log.clone(), source.clone(), TsOptions::default());
        let options = Options {
            unsupported_js_features: crate::internal::compat::JsFeature::BIGINT,
            original_target_env: "es2019".into(),
            ..Options::default()
        };
        let mut core = super::ParserCore::new_with_log(source, options, log.clone());
        let expression = parse_expression(&mut core, &mut lexer, Precedence::Lowest, true);
        assert!(matches!(
            expression.data.as_deref(),
            Some(ExprData::BigInt(value)) if value == "0xCAFEBABE"
        ));
        let messages = log.done();
        assert!(messages.is_empty());
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn parses_prefix_suffix_binary_and_conditional_as_one_expression() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"base?.method(1 + 2 * 3) ? 4 : 5"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let expression = parse_expression(&mut core, &mut lexer, Precedence::Lowest, true);
        let Some(ExprData::If(conditional)) = expression.data.as_deref() else {
            panic!("expected conditional");
        };
        let Some(ExprData::Call(call)) = conditional.test.data.as_deref() else {
            panic!("expected call");
        };
        let Some(ExprData::Binary(add)) = call.args[0].data.as_deref() else {
            panic!("expected addition");
        };
        assert_eq!(add.op, OpCode::BinaryAdd);
        assert!(matches!(
            add.right.data.as_deref(),
            Some(ExprData::Binary(multiply)) if multiply.op == OpCode::BinaryMultiply
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn respects_in_suppression() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"1 in 2"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let expression = parse_expression(&mut core, &mut lexer, Precedence::Lowest, false);
        assert!(matches!(
            expression.data.as_deref(),
            Some(ExprData::Number(_))
        ));
        assert_eq!(lexer.token, Token::In);
    }

    #[test]
    fn parses_new_with_member_target_and_constructor_arguments() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"new namespace.Constructor(1 + 2)"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let expression = parse_expression(&mut core, &mut lexer, Precedence::Lowest, true);
        let Some(ExprData::New(new)) = expression.data.as_deref() else {
            panic!("expected new expression");
        };
        assert!(matches!(new.target.data.as_deref(), Some(ExprData::Dot(_))));
        assert!(matches!(
            new.args[0].data.as_deref(),
            Some(ExprData::Binary(binary)) if binary.op == OpCode::BinaryAdd
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn reports_optional_chain_in_new_target() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"new namespace?.Constructor()"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log.clone(), source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new_with_log(source, Options::default(), log.clone());
        let expression = parse_expression(&mut core, &mut lexer, Precedence::Lowest, true);
        assert!(matches!(expression.data.as_deref(), Some(ExprData::New(_))));
        assert_eq!(log.peek().len(), 1);
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn integrates_await_and_yield_precedence() {
        for (text, policy, expected_outer, expected_inner) in [
            (
                "await 1 + 2",
                AwaitOrYield::AllowExpression,
                OpCode::BinaryAdd,
                "await",
            ),
            (
                "yield 1 + 2",
                AwaitOrYield::AllowExpression,
                OpCode::BinaryAdd,
                "yield",
            ),
        ] {
            let log = Log::new_defer(DeferLogKind::All, HashMap::new());
            let source = Source {
                contents: Arc::from(text.as_bytes()),
                ..Source::default()
            };
            let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
            let mut core = super::ParserCore::new(source, Options::default());
            if expected_inner == "await" {
                core.fn_or_arrow_data_parse.await_policy = policy;
            } else {
                core.fn_or_arrow_data_parse.yield_policy = policy;
            }
            let expression = parse_expression(&mut core, &mut lexer, Precedence::Lowest, true);
            match expected_inner {
                "await" => {
                    let Some(ExprData::Binary(binary)) = expression.data.as_deref() else {
                        panic!("expected outer addition");
                    };
                    assert_eq!(binary.op, expected_outer);
                    assert!(matches!(
                        binary.left.data.as_deref(),
                        Some(ExprData::Await(_))
                    ));
                }
                "yield" => {
                    let Some(ExprData::Yield(yield_expr)) = expression.data.as_deref() else {
                        panic!("expected outer yield");
                    };
                    assert!(matches!(
                        yield_expr.value_or_nil.data.as_deref(),
                        Some(ExprData::Binary(binary)) if binary.op == expected_outer
                    ));
                }
                _ => unreachable!(),
            }
            assert_eq!(lexer.token, Token::EndOfFile);
        }
    }

    #[test]
    fn integrates_private_brand_check_with_binary_in() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"#field in object"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let expression = parse_expression(&mut core, &mut lexer, Precedence::Lowest, true);
        assert!(matches!(
            expression.data.as_deref(),
            Some(ExprData::Binary(binary))
                if binary.op == OpCode::BinaryIn
                    && matches!(
                        binary.left.data.as_deref(),
                        Some(ExprData::PrivateIdentifier(_))
                    )
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn integrates_super_property_call_suffixes() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"super.method(1)"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log.clone(), source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new_with_log(source, Options::default(), log.clone());
        core.fn_or_arrow_data_parse.allow_super_property = true;
        let expression = parse_expression(&mut core, &mut lexer, Precedence::Lowest, true);
        let Some(ExprData::Call(call)) = expression.data.as_deref() else {
            panic!("expected call");
        };
        assert!(matches!(
            call.target.data.as_deref(),
            Some(ExprData::Dot(dot))
                if matches!(dot.target.data.as_deref(), Some(ExprData::Super))
        ));
        assert!(log.peek().is_empty());
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn integrates_object_literal_property_expressions() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"{a: 1 + 2, [key]: value, shorthand, ...rest}"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let expression = parse_expression(&mut core, &mut lexer, Precedence::Lowest, true);
        let Some(ExprData::Object(object)) = expression.data.as_deref() else {
            panic!("expected object");
        };
        assert_eq!(object.properties.len(), 4);
        assert!(matches!(
            object.properties[0].value_or_nil.data.as_deref(),
            Some(ExprData::Binary(binary)) if binary.op == OpCode::BinaryAdd
        ));
        assert_eq!(
            object.properties[3].kind,
            crate::internal::js_ast::PropertyKind::Spread
        );
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn integrates_function_expression_bodies_and_call_suffixes() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"function named(a) { return a + 1 }(2)"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let expression = parse_expression(&mut core, &mut lexer, Precedence::Lowest, true);
        let Some(ExprData::Call(call)) = expression.data.as_deref() else {
            panic!("expected call");
        };
        let Some(ExprData::Function(function)) = call.target.data.as_deref() else {
            panic!("expected function target");
        };
        assert_eq!(function.function.args.len(), 1);
        assert!(matches!(
            function.function.body.block.statements[0]
                .data
                .as_deref(),
            Some(crate::internal::js_ast::StmtData::Return(return_stmt))
                if matches!(
                    return_stmt.value_or_nil.data.as_deref(),
                    Some(ExprData::Binary(binary)) if binary.op == OpCode::BinaryAdd
                )
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn integrates_async_function_expression_call_suffix() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"async function() { await work }()"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let expression = parse_expression(&mut core, &mut lexer, Precedence::Lowest, true);
        assert!(matches!(
            expression.data.as_deref(),
            Some(ExprData::Call(call))
                if matches!(
                    call.target.data.as_deref(),
                    Some(ExprData::Function(function)) if function.function.is_async
                )
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn rejects_unparenthesized_nullish_logical_mixing_only() {
        for (text, error_count) in [
            ("a ?? b || c", 1),
            ("a && b ?? c", 1),
            ("a ?? (b || c)", 0),
            ("(a && b) ?? c", 0),
        ] {
            let log = Log::new_defer(DeferLogKind::All, HashMap::new());
            let source = Source {
                contents: Arc::from(text.as_bytes()),
                ..Source::default()
            };
            let mut lexer = Lexer::new(log.clone(), source.clone(), TsOptions::default());
            let mut core = super::ParserCore::new_with_log(source, Options::default(), log.clone());
            let _ = parse_expression(&mut core, &mut lexer, Precedence::Lowest, true);
            assert_eq!(log.peek().len(), error_count, "{text}");
        }
    }
}
