#![allow(dead_code)]

use crate::internal::{
    js_ast::{
        Binding, BindingData, BlockStmt, BreakStmt, ContinueStmt, Decl, DoWhileStmt, Expr,
        ExprData, ExprStmt, IdentifierBinding, IdentifierExpr, IfStmt, LocalKind, LocalStmt,
        Precedence, ReturnStmt, Stmt, StmtData, ThrowStmt, WhileStmt, WithStmt,
    },
    js_lexer::{Lexer, Token},
    logger::{Loc, Range},
};

use super::{
    parser_core::ParserCore,
    syntax_expression::{parse_expression, parse_expression_suffix},
};

pub(crate) fn parse_block(core: &mut ParserCore, lexer: &mut Lexer) -> (Loc, BlockStmt) {
    let loc = lexer.loc();
    lexer.expect(Token::OpenBrace);
    let mut statements = Vec::new();
    while lexer.token != Token::CloseBrace {
        statements.push(parse_statement(core, lexer));
    }
    let close_brace_loc = lexer.loc();
    lexer.expect(Token::CloseBrace);
    (
        loc,
        BlockStmt {
            statements,
            close_brace_loc,
        },
    )
}

#[allow(clippy::too_many_lines)]
pub(crate) fn parse_statement(core: &mut ParserCore, lexer: &mut Lexer) -> Stmt {
    let loc = lexer.loc();
    if lexer.is_contextual_keyword(b"let") {
        let reference = core.store_name_in_ref(lexer.identifier.clone());
        lexer.next();
        if matches!(
            lexer.token,
            Token::Identifier | Token::OpenBracket | Token::OpenBrace
        ) {
            return parse_local_declarations(core, lexer, loc, LocalKind::Let);
        }
        let value = parse_expression_suffix(
            core,
            lexer,
            Expr::new(
                loc,
                ExprData::Identifier(IdentifierExpr {
                    reference,
                    ..IdentifierExpr::default()
                }),
            ),
            Precedence::Lowest,
            true,
        );
        lexer.expect_or_insert_semicolon();
        return Stmt::new(
            loc,
            StmtData::Expr(ExprStmt {
                value,
                ..ExprStmt::default()
            }),
        );
    }

    match lexer.token {
        Token::Semicolon => {
            lexer.next();
            Stmt::new(loc, StmtData::Empty)
        }
        Token::OpenBrace => {
            let (_, block) = parse_block(core, lexer);
            Stmt::new(loc, StmtData::Block(block))
        }
        Token::If => {
            lexer.next();
            lexer.expect(Token::OpenParen);
            let test = parse_expression(core, lexer, Precedence::Lowest, true);
            lexer.expect(Token::CloseParen);
            let is_single_line_yes = !lexer.has_newline_before && lexer.token != Token::OpenBrace;
            let yes = parse_statement(core, lexer);
            let (no_or_nil, is_single_line_no) = if lexer.token == Token::Else {
                lexer.next();
                let is_single_line = !lexer.has_newline_before && lexer.token != Token::OpenBrace;
                (parse_statement(core, lexer), is_single_line)
            } else {
                (Stmt::default(), false)
            };
            Stmt::new(
                loc,
                StmtData::If(IfStmt {
                    test,
                    yes,
                    no_or_nil,
                    is_single_line_yes,
                    is_single_line_no,
                }),
            )
        }
        Token::Do => {
            lexer.next();
            let body = parse_statement(core, lexer);
            lexer.expect(Token::While);
            lexer.expect(Token::OpenParen);
            let test = parse_expression(core, lexer, Precedence::Lowest, true);
            lexer.expect(Token::CloseParen);
            if lexer.token == Token::Semicolon {
                lexer.next();
            }
            Stmt::new(loc, StmtData::DoWhile(DoWhileStmt { body, test }))
        }
        Token::While => {
            lexer.next();
            lexer.expect(Token::OpenParen);
            let test = parse_expression(core, lexer, Precedence::Lowest, true);
            lexer.expect(Token::CloseParen);
            let is_single_line_body = !lexer.has_newline_before && lexer.token != Token::OpenBrace;
            let body = parse_statement(core, lexer);
            Stmt::new(
                loc,
                StmtData::While(WhileStmt {
                    test,
                    body,
                    is_single_line_body,
                }),
            )
        }
        Token::Var => {
            lexer.next();
            parse_local_declarations(core, lexer, loc, LocalKind::Var)
        }
        Token::Const => {
            lexer.next();
            parse_local_declarations(core, lexer, loc, LocalKind::Const)
        }
        Token::Break => {
            lexer.next();
            let label = parse_optional_label(core, lexer);
            lexer.expect_or_insert_semicolon();
            Stmt::new(loc, StmtData::Break(BreakStmt { label }))
        }
        Token::Continue => {
            lexer.next();
            let label = parse_optional_label(core, lexer);
            lexer.expect_or_insert_semicolon();
            Stmt::new(loc, StmtData::Continue(ContinueStmt { label }))
        }
        Token::With => {
            lexer.next();
            lexer.expect(Token::OpenParen);
            let value = parse_expression(core, lexer, Precedence::Lowest, true);
            let body_loc = lexer.loc();
            lexer.expect(Token::CloseParen);
            let is_single_line_body = !lexer.has_newline_before && lexer.token != Token::OpenBrace;
            let body = parse_statement(core, lexer);
            Stmt::new(
                loc,
                StmtData::With(WithStmt {
                    value,
                    body,
                    body_loc,
                    is_single_line_body,
                }),
            )
        }
        Token::Return => {
            if core.fn_or_arrow_data_parse.is_return_disallowed {
                core.add_error_range(lexer.range(), "A return statement cannot be used here:");
            }
            lexer.next();
            let value_or_nil = if lexer.token != Token::Semicolon
                && !lexer.has_newline_before
                && lexer.token != Token::CloseBrace
                && lexer.token != Token::EndOfFile
            {
                parse_expression(core, lexer, Precedence::Lowest, true)
            } else {
                Expr::default()
            };
            lexer.expect_or_insert_semicolon();
            Stmt::new(loc, StmtData::Return(ReturnStmt { value_or_nil }))
        }
        Token::Throw => {
            lexer.next();
            let value = if lexer.has_newline_before {
                let end_loc = Loc {
                    start: loc.start + 5,
                };
                core.add_error_range(
                    Range {
                        loc: end_loc,
                        len: 0,
                    },
                    "Unexpected newline after \"throw\"",
                );
                Expr::new(end_loc, ExprData::Null)
            } else {
                parse_expression(core, lexer, Precedence::Lowest, true)
            };
            lexer.expect_or_insert_semicolon();
            Stmt::new(loc, StmtData::Throw(ThrowStmt { value }))
        }
        Token::Debugger => {
            lexer.next();
            lexer.expect_or_insert_semicolon();
            Stmt::new(loc, StmtData::Debugger)
        }
        _ => {
            let value = parse_expression(core, lexer, Precedence::Lowest, true);
            lexer.expect_or_insert_semicolon();
            Stmt::new(
                loc,
                StmtData::Expr(ExprStmt {
                    value,
                    ..ExprStmt::default()
                }),
            )
        }
    }
}

fn parse_local_declarations(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    loc: Loc,
    kind: LocalKind,
) -> Stmt {
    let mut declarations = Vec::new();
    loop {
        if lexer.token != Token::Identifier {
            lexer.expected(Token::Identifier);
        }
        let binding_loc = lexer.loc();
        let binding_range = lexer.range();
        let name = lexer.identifier.clone();
        let name_text = String::from_utf8(name.string.clone())
            .expect("binding identifiers must be valid UTF-8");
        if kind != LocalKind::Var && name_text == "let" {
            core.add_error_range(binding_range, "Cannot use \"let\" as an identifier here:");
        }
        let binding = Binding {
            loc: binding_loc,
            data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                reference: core.store_name_in_ref(name),
            }))),
        };
        lexer.next();
        let value_or_nil = if lexer.token == Token::Equals {
            lexer.next();
            parse_expression(core, lexer, Precedence::Comma, true)
        } else {
            Expr::default()
        };
        if kind == LocalKind::Const && value_or_nil.data.is_none() {
            core.add_error_range(
                binding_range,
                format!("The constant \"{name_text}\" must be initialized"),
            );
        }
        declarations.push(Decl {
            binding,
            value_or_nil,
        });
        if lexer.token != Token::Comma {
            break;
        }
        lexer.next();
    }
    lexer.expect_or_insert_semicolon();
    Stmt::new(
        loc,
        StmtData::Local(LocalStmt {
            declarations,
            kind,
            ..LocalStmt::default()
        }),
    )
}

fn parse_optional_label(
    core: &mut ParserCore,
    lexer: &mut Lexer,
) -> Option<crate::internal::ast::LocRef> {
    if lexer.has_newline_before || lexer.token != Token::Identifier {
        return None;
    }
    let label = crate::internal::ast::LocRef {
        loc: lexer.loc(),
        reference: core.store_name_in_ref(lexer.identifier.clone()),
    };
    lexer.next();
    Some(label)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::parse_block;
    use crate::internal::{
        config::TsOptions,
        js_ast::StmtData,
        js_lexer::{Lexer, Token},
        js_parser::Options,
        logger::{DeferLogKind, Log, Source},
    };

    #[test]
    fn parses_core_function_body_statements() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"{debugger; 1 + 2; return 3;}"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let (_, block) = parse_block(&mut core, &mut lexer);
        assert_eq!(block.statements.len(), 3);
        assert!(matches!(
            block.statements[0].data.as_deref(),
            Some(StmtData::Debugger)
        ));
        assert!(matches!(
            block.statements[2].data.as_deref(),
            Some(StmtData::Return(_))
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn parses_if_else_while_and_do_while_statements() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"{if (a) while (b) work(); else do other(); while (c);}"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let (_, block) = parse_block(&mut core, &mut lexer);
        let Some(StmtData::If(if_stmt)) = block.statements[0].data.as_deref() else {
            panic!("expected if");
        };
        assert!(matches!(
            if_stmt.yes.data.as_deref(),
            Some(StmtData::While(_))
        ));
        assert!(matches!(
            if_stmt.no_or_nil.data.as_deref(),
            Some(StmtData::DoWhile(_))
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn parses_local_declarations_and_disambiguates_let_expressions() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"{var a, b = 2; let c = 3; const d = 4; let + 5;}"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let (_, block) = parse_block(&mut core, &mut lexer);
        assert!(matches!(
            block.statements[0].data.as_deref(),
            Some(StmtData::Local(local))
                if local.kind == crate::internal::js_ast::LocalKind::Var
                    && local.declarations.len() == 2
        ));
        assert!(matches!(
            block.statements[1].data.as_deref(),
            Some(StmtData::Local(local))
                if local.kind == crate::internal::js_ast::LocalKind::Let
        ));
        assert!(matches!(
            block.statements[2].data.as_deref(),
            Some(StmtData::Local(local))
                if local.kind == crate::internal::js_ast::LocalKind::Const
        ));
        assert!(matches!(
            block.statements[3].data.as_deref(),
            Some(StmtData::Expr(_))
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn reports_uninitialized_constants() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"{const missing;}"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log.clone(), source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new_with_log(source, Options::default(), log.clone());
        let _ = parse_block(&mut core, &mut lexer);
        assert_eq!(log.peek().len(), 1);
    }

    #[test]
    fn parses_break_continue_labels_and_with_bodies() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"{break outer; continue inner; with (scope) work();}"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let (_, block) = parse_block(&mut core, &mut lexer);
        assert!(matches!(
            block.statements[0].data.as_deref(),
            Some(StmtData::Break(break_stmt)) if break_stmt.label.is_some()
        ));
        assert!(matches!(
            block.statements[1].data.as_deref(),
            Some(StmtData::Continue(continue_stmt)) if continue_stmt.label.is_some()
        ));
        assert!(matches!(
            block.statements[2].data.as_deref(),
            Some(StmtData::With(_))
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }
}
