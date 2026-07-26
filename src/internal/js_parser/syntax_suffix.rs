#![allow(dead_code)]

use crate::internal::{
    js_ast::{
        CallExpr, CallKind, Expr, ExprData, IndexExpr, OpCode, OptionalChain, Precedence,
        PrivateIdentifierExpr, SpreadExpr, UnaryExpr, is_property_access,
    },
    js_lexer::{Lexer, Token},
    logger::Loc,
};

use super::{
    parser_core::{ParserCore, WasOriginallyDotOrIndex},
    syntax_literals::parse_tagged_template_suffix,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BinaryOperator {
    pub(crate) op: OpCode,
    pub(crate) precedence: Precedence,
    pub(crate) is_right_associative: bool,
}

pub(crate) fn binary_operator(token: Token) -> Option<BinaryOperator> {
    let (op, precedence, is_right_associative) = match token {
        Token::Comma => (OpCode::BinaryComma, Precedence::Comma, false),
        Token::Equals => (OpCode::BinaryAssign, Precedence::Assign, true),
        Token::PlusEquals => (OpCode::BinaryAddAssign, Precedence::Assign, true),
        Token::MinusEquals => (OpCode::BinarySubtractAssign, Precedence::Assign, true),
        Token::AsteriskEquals => (OpCode::BinaryMultiplyAssign, Precedence::Assign, true),
        Token::SlashEquals => (OpCode::BinaryDivideAssign, Precedence::Assign, true),
        Token::PercentEquals => (OpCode::BinaryRemainderAssign, Precedence::Assign, true),
        Token::AsteriskAsteriskEquals => (OpCode::BinaryPowerAssign, Precedence::Assign, true),
        Token::LessThanLessThanEquals => (OpCode::BinaryShiftLeftAssign, Precedence::Assign, true),
        Token::GreaterThanGreaterThanEquals => {
            (OpCode::BinaryShiftRightAssign, Precedence::Assign, true)
        }
        Token::GreaterThanGreaterThanGreaterThanEquals => (
            OpCode::BinaryUnsignedShiftRightAssign,
            Precedence::Assign,
            true,
        ),
        Token::AmpersandEquals => (OpCode::BinaryBitwiseAndAssign, Precedence::Assign, true),
        Token::BarEquals => (OpCode::BinaryBitwiseOrAssign, Precedence::Assign, true),
        Token::CaretEquals => (OpCode::BinaryBitwiseXorAssign, Precedence::Assign, true),
        Token::QuestionQuestionEquals => (
            OpCode::BinaryNullishCoalescingAssign,
            Precedence::Assign,
            true,
        ),
        Token::AmpersandAmpersandEquals => {
            (OpCode::BinaryLogicalAndAssign, Precedence::Assign, true)
        }
        Token::BarBarEquals => (OpCode::BinaryLogicalOrAssign, Precedence::Assign, true),
        Token::QuestionQuestion => (
            OpCode::BinaryNullishCoalescing,
            Precedence::NullishCoalescing,
            false,
        ),
        Token::BarBar => (OpCode::BinaryLogicalOr, Precedence::LogicalOr, false),
        Token::AmpersandAmpersand => (OpCode::BinaryLogicalAnd, Precedence::LogicalAnd, false),
        Token::Bar => (OpCode::BinaryBitwiseOr, Precedence::BitwiseOr, false),
        Token::Caret => (OpCode::BinaryBitwiseXor, Precedence::BitwiseXor, false),
        Token::Ampersand => (OpCode::BinaryBitwiseAnd, Precedence::BitwiseAnd, false),
        Token::EqualsEquals => (OpCode::BinaryLooseEqual, Precedence::Equals, false),
        Token::ExclamationEquals => (OpCode::BinaryLooseNotEqual, Precedence::Equals, false),
        Token::EqualsEqualsEquals => (OpCode::BinaryStrictEqual, Precedence::Equals, false),
        Token::ExclamationEqualsEquals => (OpCode::BinaryStrictNotEqual, Precedence::Equals, false),
        Token::LessThan => (OpCode::BinaryLessThan, Precedence::Compare, false),
        Token::LessThanEquals => (OpCode::BinaryLessThanOrEqual, Precedence::Compare, false),
        Token::GreaterThan => (OpCode::BinaryGreaterThan, Precedence::Compare, false),
        Token::GreaterThanEquals => (OpCode::BinaryGreaterThanOrEqual, Precedence::Compare, false),
        Token::In => (OpCode::BinaryIn, Precedence::Compare, false),
        Token::Instanceof => (OpCode::BinaryInstanceof, Precedence::Compare, false),
        Token::LessThanLessThan => (OpCode::BinaryShiftLeft, Precedence::Shift, false),
        Token::GreaterThanGreaterThan => (OpCode::BinaryShiftRight, Precedence::Shift, false),
        Token::GreaterThanGreaterThanGreaterThan => {
            (OpCode::BinaryUnsignedShiftRight, Precedence::Shift, false)
        }
        Token::Plus => (OpCode::BinaryAdd, Precedence::Add, false),
        Token::Minus => (OpCode::BinarySubtract, Precedence::Add, false),
        Token::Asterisk => (OpCode::BinaryMultiply, Precedence::Multiply, false),
        Token::Slash => (OpCode::BinaryDivide, Precedence::Multiply, false),
        Token::Percent => (OpCode::BinaryRemainder, Precedence::Multiply, false),
        Token::AsteriskAsterisk => (OpCode::BinaryPower, Precedence::Exponentiation, true),
        _ => return None,
    };
    Some(BinaryOperator {
        op,
        precedence,
        is_right_associative,
    })
}

pub(crate) fn parse_call_args(
    lexer: &mut Lexer,
    mut parse_arg: impl FnMut(&mut Lexer) -> Expr,
) -> (Vec<Expr>, Loc, bool) {
    lexer.expect(Token::OpenParen);
    let mut args = Vec::new();
    let mut is_multi_line = false;

    while lexer.token != Token::CloseParen {
        if lexer.has_newline_before {
            is_multi_line = true;
        }
        let loc = lexer.loc();
        let is_spread = lexer.token == Token::DotDotDot;
        if is_spread {
            lexer.next();
        }
        let mut argument = parse_arg(lexer);
        if is_spread {
            argument = Expr::new(loc, ExprData::Spread(SpreadExpr { value: argument }));
        }
        args.push(argument);
        if lexer.token != Token::Comma {
            break;
        }
        if lexer.has_newline_before {
            is_multi_line = true;
        }
        lexer.next();
    }

    if lexer.has_newline_before {
        is_multi_line = true;
    }
    let close_paren_loc = lexer.loc();
    lexer.expect(Token::CloseParen);
    (args, close_paren_loc, is_multi_line)
}

#[allow(clippy::too_many_lines)]
pub(crate) fn parse_high_precedence_suffix_chain(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    mut left: Expr,
    mut parse_nested: impl FnMut(&mut Lexer) -> Expr,
) -> Expr {
    let mut optional_chain = OptionalChain::None;
    loop {
        let old_optional_chain = optional_chain;
        optional_chain = OptionalChain::None;
        match lexer.token {
            Token::Dot => {
                lexer.next();
                if lexer.token == Token::PrivateIdentifier {
                    let name_loc = lexer.loc();
                    let reference = core.store_name_in_ref(lexer.identifier.clone());
                    lexer.next();
                    left = Expr::new(
                        left.loc,
                        ExprData::Index(IndexExpr {
                            target: left,
                            index: Expr::new(
                                name_loc,
                                ExprData::PrivateIdentifier(PrivateIdentifierExpr { reference }),
                            ),
                            optional_chain: old_optional_chain,
                            ..IndexExpr::default()
                        }),
                    );
                } else {
                    if !lexer.is_identifier_or_keyword() {
                        lexer.expected(Token::Identifier);
                    }
                    let name = lexer.identifier.clone();
                    let name_loc = lexer.loc();
                    lexer.next();
                    left = Expr::new(
                        left.loc,
                        core.dot_or_mangled_prop_parse(
                            left,
                            name,
                            name_loc,
                            old_optional_chain,
                            WasOriginallyDotOrIndex::Dot,
                        ),
                    );
                }
                optional_chain = old_optional_chain;
            }
            Token::QuestionDot => {
                lexer.next();
                let optional_start = OptionalChain::Start;
                match lexer.token {
                    Token::OpenBracket => {
                        lexer.next();
                        let index = parse_nested(lexer);
                        let close_bracket_loc = lexer.loc();
                        lexer.expect(Token::CloseBracket);
                        left = Expr::new(
                            left.loc,
                            ExprData::Index(IndexExpr {
                                target: left,
                                index,
                                close_bracket_loc,
                                optional_chain: optional_start,
                                ..IndexExpr::default()
                            }),
                        );
                    }
                    Token::OpenParen => {
                        let kind = if is_property_access(&left) {
                            CallKind::TargetWasOriginallyPropertyAccess
                        } else {
                            CallKind::Normal
                        };
                        let (args, close_paren_loc, is_multi_line) =
                            parse_call_args(lexer, &mut parse_nested);
                        left = Expr::new(
                            left.loc,
                            ExprData::Call(CallExpr {
                                target: left,
                                args,
                                close_paren_loc,
                                optional_chain: optional_start,
                                kind,
                                is_multi_line,
                                ..CallExpr::default()
                            }),
                        );
                    }
                    _ => {
                        if !lexer.is_identifier_or_keyword() {
                            lexer.expected(Token::Identifier);
                        }
                        let name = lexer.identifier.clone();
                        let name_loc = lexer.loc();
                        lexer.next();
                        left = Expr::new(
                            left.loc,
                            core.dot_or_mangled_prop_parse(
                                left,
                                name,
                                name_loc,
                                optional_start,
                                WasOriginallyDotOrIndex::Dot,
                            ),
                        );
                    }
                }
                optional_chain = OptionalChain::Continue;
            }
            Token::OpenBracket => {
                lexer.next();
                let index = parse_nested(lexer);
                let close_bracket_loc = lexer.loc();
                lexer.expect(Token::CloseBracket);
                left = Expr::new(
                    left.loc,
                    ExprData::Index(IndexExpr {
                        target: left,
                        index,
                        close_bracket_loc,
                        optional_chain: old_optional_chain,
                        ..IndexExpr::default()
                    }),
                );
                optional_chain = old_optional_chain;
            }
            Token::OpenParen => {
                let kind = if is_property_access(&left) {
                    CallKind::TargetWasOriginallyPropertyAccess
                } else {
                    CallKind::Normal
                };
                let (args, close_paren_loc, is_multi_line) =
                    parse_call_args(lexer, &mut parse_nested);
                left = Expr::new(
                    left.loc,
                    ExprData::Call(CallExpr {
                        target: left,
                        args,
                        close_paren_loc,
                        optional_chain: old_optional_chain,
                        kind,
                        is_multi_line,
                        ..CallExpr::default()
                    }),
                );
                optional_chain = old_optional_chain;
            }
            Token::NoSubstitutionTemplateLiteral | Token::TemplateHead => {
                left = parse_tagged_template_suffix(left, lexer, &mut parse_nested)
                    .expect("template token was checked");
            }
            Token::MinusMinus if !lexer.has_newline_before => {
                lexer.next();
                left = Expr::new(
                    left.loc,
                    ExprData::Unary(UnaryExpr {
                        value: left,
                        op: crate::internal::js_ast::OpCode::UnaryPostDecrement,
                        ..UnaryExpr::default()
                    }),
                );
            }
            Token::PlusPlus if !lexer.has_newline_before => {
                lexer.next();
                left = Expr::new(
                    left.loc,
                    ExprData::Unary(UnaryExpr {
                        value: left,
                        op: crate::internal::js_ast::OpCode::UnaryPostIncrement,
                        ..UnaryExpr::default()
                    }),
                );
            }
            _ => return left,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::{binary_operator, parse_call_args, parse_high_precedence_suffix_chain};
    use crate::internal::{
        config::TsOptions,
        js_ast::{Expr, ExprData},
        js_lexer::{Lexer, Token},
        js_parser::Options,
        logger::{DeferLogKind, Log, Source},
    };

    #[test]
    fn parses_spread_trailing_and_multiline_call_arguments() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"(\n1, ...2,\n)"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source, TsOptions::default());
        let (args, close, multiline) = parse_call_args(&mut lexer, |lexer| {
            let loc = lexer.loc();
            let value = lexer.number;
            lexer.next();
            Expr::new(loc, ExprData::Number(value))
        });
        assert_eq!(args.len(), 2);
        assert!(matches!(args[1].data.as_deref(), Some(ExprData::Spread(_))));
        assert!(multiline);
        assert!(close.start > 0);
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn parses_member_index_call_and_postfix_suffix_chain() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"base.member[1](2)++ + 3"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let left =
            crate::internal::js_parser::syntax_literals::parse_simple_prefix(&mut core, &mut lexer)
                .expect("expected identifier");
        let result = parse_high_precedence_suffix_chain(&mut core, &mut lexer, left, |lexer| {
            let loc = lexer.loc();
            let value = lexer.number;
            lexer.next();
            Expr::new(loc, ExprData::Number(value))
        });
        let Some(ExprData::Unary(postfix)) = result.data.as_deref() else {
            panic!("expected postfix expression");
        };
        assert_eq!(
            postfix.op,
            crate::internal::js_ast::OpCode::UnaryPostIncrement
        );
        assert!(matches!(
            postfix.value.data.as_deref(),
            Some(ExprData::Call(_))
        ));
        assert_eq!(lexer.token, Token::Plus);
    }

    #[test]
    fn binary_operator_table_matches_precedence_and_associativity() {
        let add = binary_operator(Token::Plus).expect("plus is binary");
        assert_eq!(add.op, crate::internal::js_ast::OpCode::BinaryAdd);
        assert_eq!(add.precedence, crate::internal::js_ast::Precedence::Add);
        assert!(!add.is_right_associative);

        let power = binary_operator(Token::AsteriskAsterisk).expect("power is binary");
        assert_eq!(
            power.precedence,
            crate::internal::js_ast::Precedence::Exponentiation
        );
        assert!(power.is_right_associative);

        let assign =
            binary_operator(Token::QuestionQuestionEquals).expect("nullish assignment is binary");
        assert_eq!(
            assign.precedence,
            crate::internal::js_ast::Precedence::Assign
        );
        assert!(assign.is_right_associative);
        assert!(binary_operator(Token::CloseParen).is_none());
    }
}
