#![allow(dead_code)]

use crate::internal::{
    js_ast::{
        BlockStmt, Expr, ExprData, ExprStmt, Precedence, ReturnStmt, Stmt, StmtData, ThrowStmt,
    },
    js_lexer::{Lexer, Token},
    logger::{Loc, Range},
};

use super::{parser_core::ParserCore, syntax_expression::parse_expression};

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

pub(crate) fn parse_statement(core: &mut ParserCore, lexer: &mut Lexer) -> Stmt {
    let loc = lexer.loc();
    match lexer.token {
        Token::Semicolon => {
            lexer.next();
            Stmt::new(loc, StmtData::Empty)
        }
        Token::OpenBrace => {
            let (_, block) = parse_block(core, lexer);
            Stmt::new(loc, StmtData::Block(block))
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
}
