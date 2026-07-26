#![allow(dead_code)]

use crate::internal::{
    js_ast::{BinaryExpr, Expr, ExprData, IfExpr, Precedence},
    js_lexer::{Lexer, Token},
};

use super::{
    parser_core::ParserCore,
    syntax_import::parse_import_prefix,
    syntax_literals::{
        parse_array_prefix, parse_big_int_or_string_if_unsupported, parse_numeric_literal,
        parse_regular_expression_literal, parse_simple_prefix, parse_string_literal,
        parse_unary_prefix, parse_untagged_template_prefix,
    },
    syntax_suffix::{binary_operator, parse_high_precedence_suffix_chain},
};

pub(crate) fn parse_expression(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    minimum_precedence: Precedence,
    allow_in: bool,
) -> Expr {
    let mut left = parse_prefix(core, lexer, allow_in);
    left = parse_high_precedence_suffix_chain(core, lexer, left, |core, lexer| {
        parse_expression(core, lexer, Precedence::Lowest, true)
    });

    let mut previous_operator = None;
    loop {
        if lexer.token == Token::Question && Precedence::Conditional > minimum_precedence {
            lexer.next();
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

fn parse_prefix(core: &mut ParserCore, lexer: &mut Lexer, allow_in: bool) -> Expr {
    if let Some(expr) = parse_simple_prefix(core, lexer) {
        return expr;
    }
    match lexer.token {
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
            let expr = parse_big_int_or_string_if_unsupported(core, lexer);
            lexer.next();
            expr
        }
        Token::Slash | Token::SlashEquals => parse_regular_expression_literal(lexer),
        Token::OpenBracket => parse_array_prefix(lexer, |lexer| {
            parse_expression(core, lexer, Precedence::Comma, true)
        })
        .expect("array token was checked"),
        Token::OpenParen => {
            lexer.next();
            let expr = parse_expression(core, lexer, Precedence::Lowest, true);
            lexer.expect(Token::CloseParen);
            expr
        }
        _ => parse_unary_prefix(lexer, |lexer| {
            parse_expression(core, lexer, Precedence::Prefix, allow_in)
        })
        .unwrap_or_else(|| lexer.unexpected()),
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
        js_parser::Options,
        logger::{DeferLogKind, Log, Source},
    };

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
