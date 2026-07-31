#![allow(dead_code)]

use crate::internal::{
    compat::JsFeature,
    js_ast::{AwaitExpr, Expr, ExprData, IdentifierExpr, Precedence, YieldExpr},
    js_lexer::{Lexer, Token},
};

use super::{parser_core::ParserCore, parser_types::AwaitOrYield};

pub(crate) fn parse_await_or_yield_prefix(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    minimum_precedence: Precedence,
    mut parse_value: impl FnMut(&mut ParserCore, &mut Lexer, Precedence) -> Expr,
) -> Option<Expr> {
    if lexer.token != Token::Identifier {
        return None;
    }

    let is_await = lexer.identifier.string == b"await";
    let is_yield = lexer.identifier.string == b"yield";
    if !is_await && !is_yield {
        return None;
    }

    let loc = lexer.loc();
    let name_range = lexer.range();
    let is_unescaped = lexer.raw() == lexer.identifier.string;
    let reference = core.store_name_in_ref(lexer.identifier.clone());
    lexer.next();

    let policy = if is_await {
        core.fn_or_arrow_data_parse.await_policy
    } else {
        core.fn_or_arrow_data_parse.yield_policy
    };
    match policy {
        AwaitOrYield::ForbidAll => {
            let keyword = if is_await { "await" } else { "yield" };
            core.add_error_range(
                name_range,
                format!("The keyword \"{keyword}\" cannot be used here:"),
            );
        }
        AwaitOrYield::AllowExpression if !is_unescaped => {
            let keyword = if is_await { "await" } else { "yield" };
            core.add_error_range(
                name_range,
                format!("The keyword \"{keyword}\" cannot be escaped"),
            );
        }
        AwaitOrYield::AllowExpression if is_await => {
            if !core.is_inside_function_scope() {
                core.mark_syntax_feature(JsFeature::TOP_LEVEL_AWAIT, name_range);
                if core.top_level_await_keyword.len == 0 {
                    core.top_level_await_keyword = name_range;
                }
            }
            let value = parse_value(core, lexer, Precedence::Prefix);
            if lexer.token == Token::AsteriskAsterisk {
                lexer.unexpected();
            }
            return Some(Expr::new(loc, ExprData::Await(AwaitExpr { value })));
        }
        AwaitOrYield::AllowExpression => {
            if minimum_precedence > Precedence::Assign {
                core.add_error_range(
                    name_range,
                    "Cannot use a \"yield\" expression here without parentheses:",
                );
            }
            return Some(parse_yield_expr(core, lexer, loc, &mut parse_value));
        }
        AwaitOrYield::AllowIdentifier if is_await => {
            lexer.previous_token_was_await_keyword = true;
            lexer.await_keyword_loc = loc;
        }
        AwaitOrYield::AllowIdentifier if should_recover_yield_expression(lexer) => {
            core.add_error_range(
                name_range,
                "Cannot use \"yield\" outside a generator function",
            );
            return Some(parse_yield_expr(core, lexer, loc, &mut parse_value));
        }
        AwaitOrYield::AllowIdentifier => {}
    }

    Some(Expr::new(
        loc,
        ExprData::Identifier(IdentifierExpr {
            reference,
            ..IdentifierExpr::default()
        }),
    ))
}

fn parse_yield_expr(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    loc: crate::internal::logger::Loc,
    parse_value: &mut impl FnMut(&mut ParserCore, &mut Lexer, Precedence) -> Expr,
) -> Expr {
    let is_star = lexer.token == Token::Asterisk;
    if is_star && !lexer.has_newline_before {
        lexer.next();
    }

    let value_or_nil = if is_star
        || (!lexer.has_newline_before
            && !matches!(
                lexer.token,
                Token::CloseBrace
                    | Token::CloseBracket
                    | Token::CloseParen
                    | Token::Colon
                    | Token::Comma
                    | Token::Semicolon
            )) {
        parse_value(core, lexer, Precedence::Yield)
    } else {
        Expr::default()
    };

    Expr::new(
        loc,
        ExprData::Yield(YieldExpr {
            value_or_nil,
            is_star,
        }),
    )
}

fn should_recover_yield_expression(lexer: &Lexer) -> bool {
    !lexer.has_newline_before
        && matches!(
            lexer.token,
            Token::Null
                | Token::Identifier
                | Token::False
                | Token::True
                | Token::NumericLiteral
                | Token::BigIntegerLiteral
                | Token::StringLiteral
        )
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::parse_await_or_yield_prefix;
    use crate::internal::{
        config::TsOptions,
        js_ast::{Expr, ExprData, Precedence},
        js_lexer::{Lexer, Token},
        js_parser::{Options, parser_types::AwaitOrYield},
        logger::{DeferLogKind, Log, Source},
    };

    fn parse_number(_: &mut super::ParserCore, lexer: &mut Lexer, _: Precedence) -> Expr {
        let loc = lexer.loc();
        let value = lexer.number;
        lexer.next();
        Expr::new(loc, ExprData::Number(value))
    }

    fn setup(text: &[u8]) -> (Log, Lexer, super::ParserCore) {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(text),
            ..Source::default()
        };
        let lexer = Lexer::new(log.clone(), source.clone(), TsOptions::default());
        let core = super::ParserCore::new_with_log(source, Options::default(), log.clone());
        (log, lexer, core)
    }

    #[test]
    fn parses_await_expression_when_allowed() {
        let (_, mut lexer, mut core) = setup(b"await 1");
        core.fn_or_arrow_data_parse.await_policy = AwaitOrYield::AllowExpression;
        let expr =
            parse_await_or_yield_prefix(&mut core, &mut lexer, Precedence::Lowest, parse_number)
                .expect("expected await");
        assert!(matches!(expr.data.as_deref(), Some(ExprData::Await(_))));
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn parses_yield_star_and_plain_yield() {
        for (text, is_star, has_value) in [
            (&b"yield* 1"[..], true, true),
            (&b"yield 1"[..], false, true),
            (&b"yield;"[..], false, false),
        ] {
            let (_, mut lexer, mut core) = setup(text);
            core.fn_or_arrow_data_parse.yield_policy = AwaitOrYield::AllowExpression;
            let expr = parse_await_or_yield_prefix(
                &mut core,
                &mut lexer,
                Precedence::Lowest,
                parse_number,
            )
            .expect("expected yield");
            let Some(ExprData::Yield(yield_expr)) = expr.data.as_deref() else {
                panic!("expected yield expression");
            };
            assert_eq!(yield_expr.is_star, is_star);
            assert_eq!(yield_expr.value_or_nil.data.is_some(), has_value);
        }
    }

    #[test]
    fn recovers_yield_outside_generator() {
        let (log, mut lexer, mut core) = setup(b"yield 1");
        let expr =
            parse_await_or_yield_prefix(&mut core, &mut lexer, Precedence::Lowest, parse_number)
                .expect("expected recovered yield");
        assert!(matches!(expr.data.as_deref(), Some(ExprData::Yield(_))));
        assert_eq!(log.peek().len(), 1);
    }
}
