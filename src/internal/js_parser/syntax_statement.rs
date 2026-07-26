#![allow(dead_code)]

use crate::internal::{
    js_ast::{
        Binding, BindingData, BlockStmt, BreakStmt, Catch, ContinueStmt, Decl, DoWhileStmt, Expr,
        ExprData, ExprStmt, Finally, ForInStmt, ForOfStmt, ForStmt, IdentifierBinding,
        IdentifierExpr, IfStmt, LocalKind, LocalStmt, Precedence, ReturnStmt, Stmt, StmtData,
        SwitchCase, SwitchStmt, ThrowStmt, TryStmt, WhileStmt, WithStmt,
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
            return parse_local_declarations(core, lexer, loc, LocalKind::Let, true, true, true);
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
            parse_local_declarations(core, lexer, loc, LocalKind::Var, true, true, true)
        }
        Token::Const => {
            lexer.next();
            parse_local_declarations(core, lexer, loc, LocalKind::Const, true, true, true)
        }
        Token::For => parse_for_statement(core, lexer, loc),
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
        Token::Try => {
            lexer.next();
            let (block_loc, block) = parse_block(core, lexer);
            let catch = if lexer.token == Token::Catch {
                let catch_loc = lexer.loc();
                lexer.next();
                let binding_or_nil = if lexer.token == Token::OpenBrace {
                    if core
                        .options
                        .unsupported_js_features
                        .contains(crate::internal::compat::JsFeature::OPTIONAL_CATCH_BINDING)
                    {
                        Binding {
                            loc: lexer.loc(),
                            data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                                reference: core
                                    .new_symbol(crate::internal::ast::SymbolKind::Other, "e"),
                            }))),
                        }
                    } else {
                        Binding::default()
                    }
                } else {
                    lexer.expect(Token::OpenParen);
                    let binding_loc = lexer.loc();
                    if lexer.token != Token::Identifier {
                        lexer.expected(Token::Identifier);
                    }
                    let binding = Binding {
                        loc: binding_loc,
                        data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                            reference: core.store_name_in_ref(lexer.identifier.clone()),
                        }))),
                    };
                    lexer.next();
                    lexer.expect(Token::CloseParen);
                    binding
                };
                let (catch_block_loc, catch_block) = parse_block(core, lexer);
                Some(Catch {
                    binding_or_nil,
                    block: catch_block,
                    loc: catch_loc,
                    block_loc: catch_block_loc,
                })
            } else {
                None
            };
            let finally = if lexer.token == Token::Finally || catch.is_none() {
                let finally_loc = lexer.loc();
                lexer.expect(Token::Finally);
                let (_, finally_block) = parse_block(core, lexer);
                Some(Finally {
                    block: finally_block,
                    loc: finally_loc,
                })
            } else {
                None
            };
            Stmt::new(
                loc,
                StmtData::Try(TryStmt {
                    catch,
                    finally,
                    block,
                    block_loc,
                }),
            )
        }
        Token::Switch => {
            lexer.next();
            lexer.expect(Token::OpenParen);
            let test = parse_expression(core, lexer, Precedence::Lowest, true);
            lexer.expect(Token::CloseParen);
            let body_loc = lexer.loc();
            lexer.expect(Token::OpenBrace);
            let mut cases = Vec::new();
            let mut found_default = false;
            while lexer.token != Token::CloseBrace {
                let case_loc = lexer.loc();
                let value_or_nil = if lexer.token == Token::Default {
                    if found_default {
                        core.add_error_range(
                            lexer.range(),
                            "Multiple default clauses are not allowed",
                        );
                    }
                    found_default = true;
                    lexer.next();
                    lexer.expect(Token::Colon);
                    Expr::default()
                } else {
                    lexer.expect(Token::Case);
                    let value = parse_expression(core, lexer, Precedence::Lowest, true);
                    lexer.expect(Token::Colon);
                    value
                };
                let mut body = Vec::new();
                while !matches!(
                    lexer.token,
                    Token::CloseBrace | Token::Case | Token::Default
                ) {
                    body.push(parse_statement(core, lexer));
                }
                cases.push(SwitchCase {
                    value_or_nil,
                    body,
                    loc: case_loc,
                });
            }
            let close_brace_loc = lexer.loc();
            lexer.expect(Token::CloseBrace);
            Stmt::new(
                loc,
                StmtData::Switch(SwitchStmt {
                    test,
                    cases,
                    body_loc,
                    close_brace_loc,
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
    consume_semicolon: bool,
    require_const_initializer: bool,
    allow_in: bool,
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
            parse_expression(core, lexer, Precedence::Comma, allow_in)
        } else {
            Expr::default()
        };
        if require_const_initializer && kind == LocalKind::Const && value_or_nil.data.is_none() {
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
    if consume_semicolon {
        lexer.expect_or_insert_semicolon();
    }
    Stmt::new(
        loc,
        StmtData::Local(LocalStmt {
            declarations,
            kind,
            ..LocalStmt::default()
        }),
    )
}

#[allow(clippy::too_many_lines)]
fn parse_for_statement(core: &mut ParserCore, lexer: &mut Lexer, loc: Loc) -> Stmt {
    lexer.expect(Token::For);
    let mut await_range = Range::default();
    if lexer.is_contextual_keyword(b"await") {
        await_range = lexer.range();
        if core.fn_or_arrow_data_parse.await_policy
            != super::parser_types::AwaitOrYield::AllowExpression
        {
            core.add_error_range(
                await_range,
                "Cannot use \"await\" outside an async function",
            );
            await_range = Range::default();
        }
        lexer.next();
    }
    lexer.expect(Token::OpenParen);

    let init_loc = lexer.loc();
    let init_or_nil = match lexer.token {
        Token::Semicolon => Stmt::default(),
        Token::Var => {
            lexer.next();
            parse_local_declarations(core, lexer, init_loc, LocalKind::Var, false, false, false)
        }
        Token::Const => {
            lexer.next();
            parse_local_declarations(core, lexer, init_loc, LocalKind::Const, false, false, false)
        }
        _ if lexer.is_contextual_keyword(b"let") => {
            let let_reference = core.store_name_in_ref(lexer.identifier.clone());
            lexer.next();
            if matches!(
                lexer.token,
                Token::Identifier | Token::OpenBracket | Token::OpenBrace
            ) {
                parse_local_declarations(core, lexer, init_loc, LocalKind::Let, false, false, false)
            } else {
                Stmt::new(
                    init_loc,
                    StmtData::Expr(ExprStmt {
                        value: parse_expression_suffix(
                            core,
                            lexer,
                            Expr::new(
                                init_loc,
                                ExprData::Identifier(IdentifierExpr {
                                    reference: let_reference,
                                    ..IdentifierExpr::default()
                                }),
                            ),
                            Precedence::Lowest,
                            false,
                        ),
                        ..ExprStmt::default()
                    }),
                )
            }
        }
        _ => Stmt::new(
            init_loc,
            StmtData::Expr(ExprStmt {
                value: parse_expression(core, lexer, Precedence::Lowest, false),
                ..ExprStmt::default()
            }),
        ),
    };

    if lexer.is_contextual_keyword(b"of") || await_range.len > 0 {
        if !lexer.is_contextual_keyword(b"of") {
            lexer.expected_string("\"of\"");
        }
        validate_loop_declaration(core, &init_or_nil, "of", false);
        lexer.next();
        let value = parse_expression(core, lexer, Precedence::Comma, true);
        lexer.expect(Token::CloseParen);
        let is_single_line_body = !lexer.has_newline_before && lexer.token != Token::OpenBrace;
        let body = parse_statement(core, lexer);
        return Stmt::new(
            loc,
            StmtData::ForOf(ForOfStmt {
                init: init_or_nil,
                value,
                body,
                await_range,
                is_single_line_body,
            }),
        );
    }

    if lexer.token == Token::In {
        let is_var = matches!(
            init_or_nil.data.as_deref(),
            Some(StmtData::Local(local)) if local.kind == LocalKind::Var
        );
        validate_loop_declaration(core, &init_or_nil, "in", is_var);
        lexer.next();
        let value = parse_expression(core, lexer, Precedence::Lowest, true);
        lexer.expect(Token::CloseParen);
        let is_single_line_body = !lexer.has_newline_before && lexer.token != Token::OpenBrace;
        let body = parse_statement(core, lexer);
        return Stmt::new(
            loc,
            StmtData::ForIn(ForInStmt {
                init: init_or_nil,
                value,
                body,
                is_single_line_body,
            }),
        );
    }

    lexer.expect(Token::Semicolon);
    require_for_const_initializers(core, &init_or_nil);
    let test_or_nil = if lexer.token == Token::Semicolon {
        Expr::default()
    } else {
        parse_expression(core, lexer, Precedence::Lowest, true)
    };
    lexer.expect(Token::Semicolon);
    let update_or_nil = if lexer.token == Token::CloseParen {
        Expr::default()
    } else {
        parse_expression(core, lexer, Precedence::Lowest, true)
    };
    lexer.expect(Token::CloseParen);
    let is_single_line_body = !lexer.has_newline_before && lexer.token != Token::OpenBrace;
    let body = parse_statement(core, lexer);
    Stmt::new(
        loc,
        StmtData::For(ForStmt {
            init_or_nil,
            test_or_nil,
            update_or_nil,
            body,
            is_single_line_body,
            ..ForStmt::default()
        }),
    )
}

fn validate_loop_declaration(
    core: &mut ParserCore,
    init: &Stmt,
    loop_type: &str,
    allow_var_initializer: bool,
) {
    let Some(StmtData::Local(local)) = init.data.as_deref() else {
        return;
    };
    if local.declarations.len() > 1 {
        core.add_error_range(
            Range {
                loc: local.declarations[0].binding.loc,
                len: 0,
            },
            format!("for-{loop_type} loops must have a single declaration"),
        );
    } else if let Some(declaration) = local.declarations.first()
        && declaration.value_or_nil.data.is_some()
        && !(allow_var_initializer && local.kind == LocalKind::Var)
    {
        core.add_error_range(
            Range {
                loc: declaration.value_or_nil.loc,
                len: 0,
            },
            format!("for-{loop_type} loop variables cannot have an initializer"),
        );
    }
}

fn require_for_const_initializers(core: &mut ParserCore, init: &Stmt) {
    let Some(StmtData::Local(local)) = init.data.as_deref() else {
        return;
    };
    if local.kind != LocalKind::Const {
        return;
    }
    for declaration in &local.declarations {
        if declaration.value_or_nil.data.is_none() {
            core.add_error_range(
                Range {
                    loc: declaration.binding.loc,
                    len: 0,
                },
                "The constant must be initialized",
            );
        }
    }
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

    #[test]
    fn parses_try_catch_finally_and_optional_catch_bindings() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                &b"{try { work() } catch (error) { recover(error) } finally { cleanup() } try {} catch {}}"[..],
            ),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let (_, block) = parse_block(&mut core, &mut lexer);
        let Some(StmtData::Try(first)) = block.statements[0].data.as_deref() else {
            panic!("expected try");
        };
        assert!(
            first
                .catch
                .as_ref()
                .is_some_and(|catch| catch.binding_or_nil.data.is_some())
        );
        assert!(first.finally.is_some());
        let Some(StmtData::Try(second)) = block.statements[1].data.as_deref() else {
            panic!("expected second try");
        };
        assert!(
            second
                .catch
                .as_ref()
                .is_some_and(|catch| catch.binding_or_nil.data.is_none())
        );
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn parses_switch_cases_and_reports_duplicate_defaults() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                &b"{switch (value) { case 1: work(); break; default: other(); default: finalWork(); }}"[..],
            ),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log.clone(), source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new_with_log(source, Options::default(), log.clone());
        let (_, block) = parse_block(&mut core, &mut lexer);
        let Some(StmtData::Switch(switch_stmt)) = block.statements[0].data.as_deref() else {
            panic!("expected switch");
        };
        assert_eq!(switch_stmt.cases.len(), 3);
        assert_eq!(switch_stmt.cases[0].body.len(), 2);
        assert_eq!(log.peek().len(), 1);
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn parses_classic_for_in_and_for_of_loops() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                &b"{for (let i = 0; i < 3; i++) work(i); for (const key in object) use(key); for (const value of values) use(value);}"[..],
            ),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let (_, block) = parse_block(&mut core, &mut lexer);
        assert!(matches!(
            block.statements[0].data.as_deref(),
            Some(StmtData::For(_))
        ));
        assert!(matches!(
            block.statements[1].data.as_deref(),
            Some(StmtData::ForIn(_))
        ));
        assert!(matches!(
            block.statements[2].data.as_deref(),
            Some(StmtData::ForOf(_))
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn parses_for_await_of_inside_async_context() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"{for await (const value of values) use(value);}"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        core.fn_or_arrow_data_parse.await_policy =
            crate::internal::js_parser::parser_types::AwaitOrYield::AllowExpression;
        let (_, block) = parse_block(&mut core, &mut lexer);
        assert!(matches!(
            block.statements[0].data.as_deref(),
            Some(StmtData::ForOf(for_of)) if for_of.await_range.len > 0
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }
}
