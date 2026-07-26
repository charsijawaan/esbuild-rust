#![allow(dead_code)]

use crate::internal::{
    js_ast::{
        CallExpr, CallKind, Expr, ExprData, IndexExpr, OptionalChain, PrivateIdentifierExpr,
        SpreadExpr, UnaryExpr, is_property_access,
    },
    js_lexer::{Lexer, Token},
    logger::Loc,
};

use super::{
    parser_core::{ParserCore, WasOriginallyDotOrIndex},
    syntax_literals::parse_tagged_template_suffix,
};

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

    use super::{parse_call_args, parse_high_precedence_suffix_chain};
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
}
