#![allow(dead_code)]

use crate::internal::{
    js_ast::{
        AwaitExpr, Binding, BindingData, BlockStmt, BreakStmt, Catch, ClassStmt, CommentStmt,
        ContinueStmt, Decl, DoWhileStmt, Expr, ExprData, ExprStmt, Finally, ForInStmt, ForOfStmt,
        ForStmt, FunctionStmt, IdentifierBinding, IdentifierExpr, IfStmt, LabelStmt, LocalKind,
        LocalStmt, Precedence, ReturnStmt, Stmt, StmtData, SwitchCase, SwitchStmt, ThrowStmt,
        TryStmt, WhileStmt, WithStmt,
    },
    js_lexer::{Lexer, Token},
    logger::{Loc, Range},
};

use super::{
    parser_core::ParserCore,
    syntax_binding::parse_binding,
    syntax_class::parse_class_prefix,
    syntax_expression::{
        parse_expression, parse_expression_suffix, parse_expression_suffix_with_flags,
    },
    syntax_function::{parse_async_statement_prefix, parse_function_declaration_prefix},
    syntax_module::{parse_export_statement, parse_import_statement},
};

pub(crate) fn parse_block(core: &mut ParserCore, lexer: &mut Lexer) -> (Loc, BlockStmt) {
    parse_block_with_scope(core, lexer, crate::internal::js_ast::ScopeKind::Block)
}

fn parse_block_without_scope(core: &mut ParserCore, lexer: &mut Lexer) -> (Loc, BlockStmt) {
    let loc = lexer.loc();
    lexer.expect(Token::OpenBrace);
    let statements = parse_statements_up_to(core, lexer, Token::CloseBrace);
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

pub(crate) fn parse_statements_up_to(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    end: Token,
) -> Vec<Stmt> {
    let mut statements = Vec::new();
    loop {
        for comment in &lexer.legal_comments_before_token {
            statements.push(Stmt::new(
                comment.loc,
                StmtData::Comment(CommentStmt {
                    text: String::from_utf8_lossy(
                        &lexer.source.comment_text_without_indent(*comment),
                    )
                    .into_owned(),
                    is_legal_comment: true,
                }),
            ));
        }
        if lexer.token == end {
            break;
        }
        statements.push(parse_statement(core, lexer));
    }
    statements
}

pub(crate) fn parse_block_with_scope(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    kind: crate::internal::js_ast::ScopeKind,
) -> (Loc, BlockStmt) {
    let loc = lexer.loc();
    core.push_scope_for_parse_pass(kind, loc);
    let block = parse_block_without_scope(core, lexer);
    core.pop_scope();
    block
}

#[allow(clippy::too_many_lines)]
pub(crate) fn parse_statement(core: &mut ParserCore, lexer: &mut Lexer) -> Stmt {
    let loc = lexer.loc();
    if let Some(statement) =
        super::syntax_typescript::parse_type_script_statement(core, lexer, false)
    {
        return statement;
    }
    if lexer.is_contextual_keyword(b"await")
        && core.fn_or_arrow_data_parse.await_policy
            == super::parser_types::AwaitOrYield::AllowExpression
    {
        let await_range = lexer.range();
        if !core.is_inside_function_scope() && core.top_level_await_keyword.len == 0 {
            core.top_level_await_keyword = await_range;
        }
        lexer.next();
        if !lexer.has_newline_before && lexer.is_contextual_keyword(b"using") {
            let using_loc = lexer.loc();
            let using_reference = core.store_name_in_ref(lexer.identifier.clone());
            lexer.next();
            if !lexer.has_newline_before && lexer.token == Token::Identifier {
                return parse_local_declarations(
                    core,
                    lexer,
                    loc,
                    LocalKind::AwaitUsing,
                    true,
                    true,
                    true,
                );
            }
            let operand = parse_expression_suffix(
                core,
                lexer,
                Expr::new(
                    using_loc,
                    ExprData::Identifier(IdentifierExpr {
                        reference: using_reference,
                        ..IdentifierExpr::default()
                    }),
                ),
                Precedence::Prefix,
                true,
            );
            if lexer.token == Token::AsteriskAsterisk {
                lexer.unexpected();
            }
            let value = parse_expression_suffix(
                core,
                lexer,
                Expr::new(loc, ExprData::Await(AwaitExpr { value: operand })),
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
        let operand = parse_expression(core, lexer, Precedence::Prefix, true);
        if lexer.token == Token::AsteriskAsterisk {
            lexer.unexpected();
        }
        let value = parse_expression_suffix(
            core,
            lexer,
            Expr::new(loc, ExprData::Await(AwaitExpr { value: operand })),
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
    if lexer.is_contextual_keyword(b"using") {
        let reference = core.store_name_in_ref(lexer.identifier.clone());
        lexer.next();
        if !lexer.has_newline_before && lexer.token == Token::Identifier {
            return parse_local_declarations(core, lexer, loc, LocalKind::Using, true, true, true);
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
    if lexer.is_contextual_keyword(b"let") {
        let name_loc = lexer.loc();
        let reference = core.store_name_in_ref(lexer.identifier.clone());
        lexer.next();
        if lexer.token == Token::Colon {
            return parse_label_statement(core, lexer, loc, name_loc, reference);
        }
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
    if lexer.is_contextual_keyword(b"async") {
        let expression =
            parse_async_statement_prefix(core, lexer).expect("async token was checked");
        if matches!(expression.data.as_deref(), Some(ExprData::Function(_))) {
            return function_declaration_from_expression(core, loc, expression);
        }
        if lexer.token == Token::Colon {
            let Some(ExprData::Identifier(identifier)) = expression.data.as_deref() else {
                unreachable!("non-function async prefix is an identifier");
            };
            return parse_label_statement(core, lexer, loc, loc, identifier.reference);
        }
        let value = parse_expression_suffix(core, lexer, expression, Precedence::Lowest, true);
        lexer.expect_or_insert_semicolon();
        return Stmt::new(
            loc,
            StmtData::Expr(ExprStmt {
                value,
                ..ExprStmt::default()
            }),
        );
    }
    if core.options.ts.parse && lexer.is_contextual_keyword(b"abstract") {
        let name_loc = lexer.loc();
        let reference = core.store_name_in_ref(lexer.identifier.clone());
        lexer.next();
        if !lexer.has_newline_before && lexer.token == Token::Class {
            let expression = parse_class_prefix(core, lexer).expect("class token was checked");
            return class_declaration_from_expression(core, loc, expression);
        }
        let value = parse_expression_suffix(
            core,
            lexer,
            Expr::new(
                name_loc,
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
    if lexer.token == Token::Identifier && !matches!(lexer.raw(), b"await" | b"yield") {
        let comment_flags = lexer.has_comment_before;
        let name_loc = lexer.loc();
        let reference = core.store_name_in_ref(lexer.identifier.clone());
        lexer.next();
        if lexer.token == Token::Colon {
            return parse_label_statement(core, lexer, loc, name_loc, reference);
        }
        let value = parse_expression_suffix_with_flags(
            core,
            lexer,
            Expr::new(
                name_loc,
                ExprData::Identifier(IdentifierExpr {
                    reference,
                    ..IdentifierExpr::default()
                }),
            ),
            Precedence::Lowest,
            true,
            comment_flags,
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
        Token::Function => {
            let expression =
                parse_function_declaration_prefix(core, lexer).expect("function token was checked");
            function_declaration_from_expression(core, loc, expression)
        }
        Token::Class => {
            let expression = parse_class_prefix(core, lexer).expect("class token was checked");
            class_declaration_from_expression(core, loc, expression)
        }
        Token::At => {
            let expression = parse_class_prefix(core, lexer).expect("decorator token was checked");
            class_declaration_from_expression(core, loc, expression)
        }
        Token::Import => parse_import_statement(core, lexer),
        Token::Export => parse_export_statement(core, lexer),
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
            if core.options.ts.parse && lexer.token == Token::Enum {
                super::syntax_typescript::parse_enum_statement(core, lexer, false)
            } else {
                parse_local_declarations(core, lexer, loc, LocalKind::Const, true, true, true)
            }
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
            core.push_scope_for_parse_pass(crate::internal::js_ast::ScopeKind::With, body_loc);
            let is_single_line_body = !lexer.has_newline_before && lexer.token != Token::OpenBrace;
            let body = parse_statement(core, lexer);
            core.pop_scope();
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
                core.push_scope_for_parse_pass(
                    crate::internal::js_ast::ScopeKind::CatchBinding,
                    catch_loc,
                );
                lexer.next();
                let mut binding_or_nil = if lexer.token == Token::OpenBrace {
                    if core
                        .options
                        .unsupported_js_features
                        .contains(crate::internal::compat::JsFeature::OPTIONAL_CATCH_BINDING)
                    {
                        let reference =
                            core.new_symbol(crate::internal::ast::SymbolKind::Other, "e");
                        core.current_scope
                            .as_ref()
                            .expect("catch binding scope")
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .generated
                            .push(reference);
                        Binding {
                            loc: lexer.loc(),
                            data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                                reference,
                            }))),
                        }
                    } else {
                        Binding::default()
                    }
                } else {
                    lexer.expect(Token::OpenParen);
                    let binding = parse_binding(core, lexer);
                    if core.options.ts.parse {
                        super::syntax_typescript::skip_type_annotation(lexer, &[Token::CloseParen]);
                    }
                    lexer.expect(Token::CloseParen);
                    binding
                };
                if let Some(binding) = binding_or_nil.data.as_deref() {
                    let kind = if matches!(binding, BindingData::Identifier(_)) {
                        crate::internal::ast::SymbolKind::CatchIdentifier
                    } else {
                        crate::internal::ast::SymbolKind::Other
                    };
                    core.declare_binding(kind, &mut binding_or_nil);
                }
                let (catch_block_loc, catch_block) = parse_block(core, lexer);
                core.pop_scope();
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
            core.push_scope_for_parse_pass(crate::internal::js_ast::ScopeKind::Block, body_loc);
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
            core.pop_scope();
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
            Stmt::new(
                loc,
                if core.options.drop_debugger {
                    StmtData::Empty
                } else {
                    StmtData::Debugger
                },
            )
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
        let binding_range = lexer.range();
        let name_text = if lexer.token == Token::Identifier {
            Some(
                String::from_utf8(lexer.identifier.string.clone())
                    .expect("binding identifiers must be valid UTF-8"),
            )
        } else {
            None
        };
        if kind != LocalKind::Var && name_text.as_deref() == Some("let") {
            core.add_error_range(binding_range, "Cannot use \"let\" as an identifier here:");
        }
        let mut binding = parse_binding(core, lexer);
        if core.options.ts.parse && lexer.token == Token::Exclamation {
            lexer.next();
        }
        if core.options.ts.parse {
            super::syntax_typescript::skip_type_annotation(
                lexer,
                &[
                    Token::Equals,
                    Token::Comma,
                    Token::Semicolon,
                    Token::In,
                    Token::CloseParen,
                ],
            );
        }
        let symbol_kind = match kind {
            LocalKind::Var => crate::internal::ast::SymbolKind::Hoisted,
            LocalKind::Const => crate::internal::ast::SymbolKind::Const,
            LocalKind::Let | LocalKind::Using | LocalKind::AwaitUsing => {
                crate::internal::ast::SymbolKind::Other
            }
        };
        core.declare_binding(symbol_kind, &mut binding);
        let value_or_nil = if lexer.token == Token::Equals {
            lexer.next();
            parse_expression(core, lexer, Precedence::Comma, allow_in)
        } else {
            Expr::default()
        };
        if require_const_initializer
            && matches!(
                kind,
                LocalKind::Const | LocalKind::Using | LocalKind::AwaitUsing
            )
            && value_or_nil.data.is_none()
        {
            core.add_error_range(
                binding_range,
                if kind == LocalKind::Const {
                    name_text.as_ref().map_or_else(
                        || "The constant must be initialized".into(),
                        |name| format!("The constant \"{name}\" must be initialized"),
                    )
                } else {
                    name_text.as_ref().map_or_else(
                        || "The declaration must be initialized".into(),
                        |name| format!("The declaration \"{name}\" must be initialized"),
                    )
                },
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

fn function_declaration_from_expression(core: &mut ParserCore, loc: Loc, expression: Expr) -> Stmt {
    let Some(data) = expression.data else {
        unreachable!("function parser always returns expression data");
    };
    let ExprData::Function(mut function) = *data else {
        unreachable!("function declaration requires a function expression");
    };
    if !function.function.has_body {
        return Stmt::new(
            loc,
            StmtData::TypeScript(crate::internal::js_ast::TypeScriptStmt::default()),
        );
    }
    if function.function.name.is_none() {
        core.add_error_range(
            Range { loc, len: 8 },
            "A function declaration must have a name",
        );
    }
    if let Some(name) = &mut function.function.name
        && ParserCore::is_stored_name_ref(name.reference)
    {
        let text = String::from_utf8_lossy(core.load_name_from_ref(name.reference)).into_owned();
        let kind = if function.function.is_async || function.function.is_generator {
            crate::internal::ast::SymbolKind::GeneratorOrAsyncFunction
        } else {
            crate::internal::ast::SymbolKind::HoistedFunction
        };
        name.reference = core.declare_symbol(kind, name.loc, &text);
    }
    Stmt::new(
        loc,
        StmtData::Function(FunctionStmt {
            function: function.function,
            is_export: false,
        }),
    )
}

pub(crate) fn class_declaration_from_expression(
    core: &mut ParserCore,
    loc: Loc,
    expression: Expr,
) -> Stmt {
    let Some(data) = expression.data else {
        unreachable!("class parser always returns expression data");
    };
    let ExprData::Class(mut class) = *data else {
        unreachable!("class declaration requires a class expression");
    };
    if class.class.name.is_none() {
        core.add_error_range(
            Range { loc, len: 5 },
            "A class declaration must have a name",
        );
    }
    if let Some(name) = &mut class.class.name
        && ParserCore::is_stored_name_ref(name.reference)
    {
        let text = String::from_utf8_lossy(core.load_name_from_ref(name.reference)).into_owned();
        name.reference =
            core.declare_symbol(crate::internal::ast::SymbolKind::Class, name.loc, &text);
    }
    Stmt::new(
        loc,
        StmtData::Class(ClassStmt {
            class: class.class,
            is_export: false,
        }),
    )
}

fn parse_label_statement(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    loc: Loc,
    name_loc: Loc,
    reference: crate::internal::ast::Ref,
) -> Stmt {
    core.push_scope_for_parse_pass(crate::internal::js_ast::ScopeKind::Label, loc);
    lexer.expect(Token::Colon);
    let is_single_line_stmt = !lexer.has_newline_before && lexer.token != Token::OpenBrace;
    let statement = parse_statement(core, lexer);
    core.pop_scope();
    Stmt::new(
        loc,
        StmtData::Label(LabelStmt {
            statement,
            name: crate::internal::ast::LocRef {
                loc: name_loc,
                reference,
            },
            is_single_line_stmt,
        }),
    )
}

#[allow(clippy::too_many_lines)]
fn parse_for_statement(core: &mut ParserCore, lexer: &mut Lexer, loc: Loc) -> Stmt {
    core.push_scope_for_parse_pass(crate::internal::js_ast::ScopeKind::Block, loc);
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
        } else if !core.is_inside_function_scope() && core.top_level_await_keyword.len == 0 {
            core.top_level_await_keyword = await_range;
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
        _ if lexer.is_contextual_keyword(b"using") => {
            let using_reference = core.store_name_in_ref(lexer.identifier.clone());
            lexer.next();
            if !lexer.has_newline_before
                && lexer.token == Token::Identifier
                && !lexer.is_contextual_keyword(b"of")
            {
                parse_local_declarations(
                    core,
                    lexer,
                    init_loc,
                    LocalKind::Using,
                    false,
                    false,
                    false,
                )
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
                                    reference: using_reference,
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
        _ if lexer.is_contextual_keyword(b"await")
            && core.fn_or_arrow_data_parse.await_policy
                == super::parser_types::AwaitOrYield::AllowExpression =>
        {
            let await_range = lexer.range();
            if !core.is_inside_function_scope() && core.top_level_await_keyword.len == 0 {
                core.top_level_await_keyword = await_range;
            }
            lexer.next();
            if !lexer.has_newline_before && lexer.is_contextual_keyword(b"using") {
                let using_loc = lexer.loc();
                let using_reference = core.store_name_in_ref(lexer.identifier.clone());
                lexer.next();
                if !lexer.has_newline_before && lexer.token == Token::Identifier {
                    parse_local_declarations(
                        core,
                        lexer,
                        init_loc,
                        LocalKind::AwaitUsing,
                        false,
                        false,
                        false,
                    )
                } else {
                    let operand = parse_expression_suffix(
                        core,
                        lexer,
                        Expr::new(
                            using_loc,
                            ExprData::Identifier(IdentifierExpr {
                                reference: using_reference,
                                ..IdentifierExpr::default()
                            }),
                        ),
                        Precedence::Prefix,
                        false,
                    );
                    if lexer.token == Token::AsteriskAsterisk {
                        lexer.unexpected();
                    }
                    Stmt::new(
                        init_loc,
                        StmtData::Expr(ExprStmt {
                            value: parse_expression_suffix(
                                core,
                                lexer,
                                Expr::new(init_loc, ExprData::Await(AwaitExpr { value: operand })),
                                Precedence::Lowest,
                                false,
                            ),
                            ..ExprStmt::default()
                        }),
                    )
                }
            } else {
                let operand = parse_expression(core, lexer, Precedence::Prefix, false);
                if lexer.token == Token::AsteriskAsterisk {
                    lexer.unexpected();
                }
                Stmt::new(
                    init_loc,
                    StmtData::Expr(ExprStmt {
                        value: parse_expression_suffix(
                            core,
                            lexer,
                            Expr::new(init_loc, ExprData::Await(AwaitExpr { value: operand })),
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
        let statement = Stmt::new(
            loc,
            StmtData::ForOf(ForOfStmt {
                init: init_or_nil,
                value,
                body,
                await_range,
                is_single_line_body,
            }),
        );
        core.pop_scope();
        return statement;
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
        let statement = Stmt::new(
            loc,
            StmtData::ForIn(ForInStmt {
                init: init_or_nil,
                value,
                body,
                is_single_line_body,
            }),
        );
        core.pop_scope();
        return statement;
    }

    lexer.expect(Token::Semicolon);
    if matches!(
        init_or_nil.data.as_deref(),
        Some(StmtData::Local(local)) if local.kind == LocalKind::AwaitUsing
    ) {
        core.add_error_range(
            Range {
                loc: init_or_nil.loc,
                len: 5,
            },
            "\"await using\" declarations are not allowed here",
        );
    }
    require_for_initializers(core, &init_or_nil);
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
    let statement = Stmt::new(
        loc,
        StmtData::For(ForStmt {
            init_or_nil,
            test_or_nil,
            update_or_nil,
            body,
            is_single_line_body,
            ..ForStmt::default()
        }),
    );
    core.pop_scope();
    statement
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
    if loop_type == "in" && local.declarations.len() == 1 {
        let message = match local.kind {
            LocalKind::Using => Some("\"using\" declarations are not allowed here"),
            LocalKind::AwaitUsing => Some("\"await using\" declarations are not allowed here"),
            _ => None,
        };
        if let Some(message) = message {
            core.add_error_range(
                Range {
                    loc: init.loc,
                    len: 5,
                },
                message,
            );
        }
    }
}

fn require_for_initializers(core: &mut ParserCore, init: &Stmt) {
    let Some(StmtData::Local(local)) = init.data.as_deref() else {
        return;
    };
    if !matches!(local.kind, LocalKind::Const | LocalKind::Using) {
        return;
    }
    for declaration in &local.declarations {
        if declaration.value_or_nil.data.is_none() {
            let name = match declaration.binding.data.as_deref() {
                Some(BindingData::Identifier(binding)) => core
                    .symbols
                    .get(
                        usize::try_from(binding.reference.inner_index)
                            .expect("symbol index must fit in usize"),
                    )
                    .map(|symbol| symbol.original_name.as_str()),
                _ => None,
            };
            core.add_error_range(
                Range {
                    loc: declaration.binding.loc,
                    len: 0,
                },
                match local.kind {
                    LocalKind::Const => name.map_or_else(
                        || "The constant must be initialized".into(),
                        |name| format!("The constant \"{name}\" must be initialized"),
                    ),
                    LocalKind::Using => name.map_or_else(
                        || "The declaration must be initialized".into(),
                        |name| format!("The declaration \"{name}\" must be initialized"),
                    ),
                    _ => unreachable!(),
                },
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
        js_ast::{BindingData, StmtData},
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

    #[test]
    fn parses_function_async_and_generator_declarations() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                &b"{function plain() {} function* generator() { yield 1 } async function task() { await work }}"[..],
            ),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let (_, block) = parse_block(&mut core, &mut lexer);
        assert!(
            block.statements.iter().all(|statement| {
                matches!(statement.data.as_deref(), Some(StmtData::Function(_)))
            })
        );
        assert!(matches!(
            block.statements[1].data.as_deref(),
            Some(StmtData::Function(function)) if function.function.is_generator
        ));
        assert!(matches!(
            block.statements[2].data.as_deref(),
            Some(StmtData::Function(function)) if function.function.is_async
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn parses_labeled_statements_with_labeled_branches() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                &b"{outer: while (condition) { continue outer; break outer; }}"[..],
            ),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let (_, block) = parse_block(&mut core, &mut lexer);
        let Some(StmtData::Label(label)) = block.statements[0].data.as_deref() else {
            panic!("expected label");
        };
        assert!(matches!(
            label.statement.data.as_deref(),
            Some(StmtData::While(_))
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn parses_class_declarations() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                &b"{class Child extends Parent { constructor() { super() } field = 1 }}"[..],
            ),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let (_, block) = parse_block(&mut core, &mut lexer);
        assert!(matches!(
            block.statements[0].data.as_deref(),
            Some(StmtData::Class(class))
                if class.class.name.is_some()
                    && class.class.extends_or_nil.data.is_some()
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn integrates_destructuring_bindings_in_declarations_and_parameters() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                &b"{const {a: renamed, b = 2, ...rest} = object; let [first, , ...tail] = items; function fn({x}, [y = 1]) { return x + y }}"[..],
            ),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let (_, block) = parse_block(&mut core, &mut lexer);
        assert!(matches!(
            block.statements[0].data.as_deref(),
            Some(StmtData::Local(local))
                if matches!(
                    local.declarations[0].binding.data.as_deref(),
                    Some(BindingData::Object(_))
                )
        ));
        assert!(matches!(
            block.statements[1].data.as_deref(),
            Some(StmtData::Local(local))
                if matches!(
                    local.declarations[0].binding.data.as_deref(),
                    Some(BindingData::Array(_))
                )
        ));
        assert!(matches!(
            block.statements[2].data.as_deref(),
            Some(StmtData::Function(function))
                if matches!(
                    function.function.args[0].binding.data.as_deref(),
                    Some(BindingData::Object(_))
                )
                    && matches!(
                        function.function.args[1].binding.data.as_deref(),
                        Some(BindingData::Array(_))
                    )
        ));
        assert_eq!(lexer.token, Token::EndOfFile);
    }
}
