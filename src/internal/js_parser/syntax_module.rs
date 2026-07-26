#![allow(dead_code)]

use crate::internal::{
    ast::{ImportKind, ImportRecord, ImportRecordFlags, LocRef, SymbolKind},
    helpers::utf16_to_string,
    js_ast::{
        ClauseItem, ExportClauseStmt, ExportDefaultStmt, ExportFromStmt, ExportStarAlias,
        ExportStarStmt, ExprStmt, ImportStmt, Precedence, Stmt, StmtData,
        generate_non_unique_name_from_path,
    },
    js_lexer::{Lexer, MaybeSubstring, Token},
    logger::{Loc, Path},
};

use super::{
    parser_core::ParserCore,
    syntax_class::parse_class_prefix,
    syntax_expression::{parse_expression, parse_expression_suffix},
    syntax_function::parse_function_declaration_prefix,
    syntax_import::parse_import_after_keyword,
};

#[allow(clippy::too_many_lines)]
pub(crate) fn parse_import_statement(core: &mut ParserCore, lexer: &mut Lexer) -> Stmt {
    let loc = lexer.loc();
    lexer.expect(Token::Import);

    if matches!(lexer.token, Token::OpenParen | Token::Dot) {
        let expression = parse_import_after_keyword(core, lexer, loc, |core, lexer| {
            parse_expression(core, lexer, Precedence::Comma, true)
        });
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
    if core.options.ts.parse && lexer.is_contextual_keyword(b"type") {
        lexer.next();
        let mut delimiters = Vec::new();
        while lexer.token != Token::StringLiteral || !delimiters.is_empty() {
            match lexer.token {
                Token::OpenBrace => delimiters.push(Token::CloseBrace),
                Token::OpenBracket => delimiters.push(Token::CloseBracket),
                Token::OpenParen => delimiters.push(Token::CloseParen),
                token if delimiters.last() == Some(&token) => {
                    delimiters.pop();
                }
                Token::EndOfFile => lexer.expected(Token::StringLiteral),
                _ => {}
            }
            lexer.next();
        }
        lexer.next();
        lexer.expect_or_insert_semicolon();
        core.has_type_script_export = true;
        return Stmt::new(
            loc,
            StmtData::TypeScript(crate::internal::js_ast::TypeScriptStmt::default()),
        );
    }
    if !core.is_current_scope_module_scope() {
        core.add_error_range(
            crate::internal::logger::Range { loc, len: 6 },
            "An import declaration can only be used at the top level of a module",
        );
    }

    let mut statement = ImportStmt::default();
    let mut was_bare = false;
    match lexer.token {
        Token::StringLiteral => was_bare = true,
        Token::Asterisk => {
            lexer.next();
            lexer.expect_contextual_keyword(b"as");
            statement.namespace_ref = core.store_name_in_ref(lexer.identifier.clone());
            statement.star_name_loc = Some(lexer.loc());
            lexer.expect(Token::Identifier);
            lexer.expect_contextual_keyword(b"from");
        }
        Token::OpenBrace => {
            let (items, is_single_line, had_type_only_items) = parse_clause(core, lexer, false);
            statement.items = Some(items);
            statement.is_single_line = is_single_line;
            was_bare = had_type_only_items && statement.items.as_ref().is_some_and(Vec::is_empty);
            lexer.expect_contextual_keyword(b"from");
        }
        Token::Identifier => {
            statement.default_name = Some(LocRef {
                loc: lexer.loc(),
                reference: core.store_name_in_ref(lexer.identifier.clone()),
            });
            lexer.next();
            if lexer.token == Token::Comma {
                lexer.next();
                match lexer.token {
                    Token::Asterisk => {
                        lexer.next();
                        lexer.expect_contextual_keyword(b"as");
                        statement.namespace_ref = core.store_name_in_ref(lexer.identifier.clone());
                        statement.star_name_loc = Some(lexer.loc());
                        lexer.expect(Token::Identifier);
                    }
                    Token::OpenBrace => {
                        let (items, is_single_line, _) = parse_clause(core, lexer, false);
                        statement.items = Some(items);
                        statement.is_single_line = is_single_line;
                    }
                    _ => lexer.unexpected(),
                }
            }
            lexer.expect_contextual_keyword(b"from");
        }
        _ => lexer.unexpected(),
    }

    let (path_range, path) = parse_path(lexer);
    lexer.expect_or_insert_semicolon();
    if core.options.ts.parse
        && was_bare
        && statement.items.as_ref().is_some_and(Vec::is_empty)
        && statement.default_name.is_none()
    {
        core.has_type_script_export = true;
        return Stmt::new(
            loc,
            StmtData::TypeScript(crate::internal::js_ast::TypeScriptStmt::default()),
        );
    }
    if statement.star_name_loc.is_none() {
        statement.namespace_ref = core.new_symbol(
            SymbolKind::Other,
            format!("import_{}", generate_non_unique_name_from_path(&path)),
        );
    }
    let flags = if was_bare {
        ImportRecordFlags::WAS_ORIGINALLY_BARE_IMPORT
    } else {
        ImportRecordFlags::default()
    };
    statement.import_record_index = add_import_record(core, path_range, path, flags);
    Stmt::new(loc, StmtData::Import(statement))
}

#[allow(clippy::too_many_lines)]
pub(crate) fn parse_export_statement(core: &mut ParserCore, lexer: &mut Lexer) -> Stmt {
    let loc = lexer.loc();
    lexer.expect(Token::Export);
    let is_namespace_scope = core.current_scope.as_ref().is_some_and(|scope| {
        scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ts_namespace
            .is_some()
    });
    if !core.is_current_scope_module_scope() && !is_namespace_scope {
        core.add_error_range(
            crate::internal::logger::Range { loc, len: 6 },
            "An export declaration can only be used at the top level of a module",
        );
    }
    if let Some(statement) =
        super::syntax_typescript::parse_type_script_statement(core, lexer, true)
    {
        core.has_type_script_export = true;
        return statement;
    }
    if core.options.ts.parse && lexer.is_contextual_keyword(b"abstract") {
        lexer.next();
        let expression =
            super::syntax_class::parse_class_prefix(core, lexer).unwrap_or_else(|| {
                lexer.expected(Token::Class);
            });
        let mut statement =
            super::syntax_statement::class_declaration_from_expression(core, loc, expression);
        mark_declaration_exported(&mut statement);
        return statement;
    }
    match lexer.token {
        Token::Var | Token::Const | Token::Function | Token::Class | Token::Enum => {
            let mut statement = super::syntax_statement::parse_statement(core, lexer);
            mark_declaration_exported(&mut statement);
            statement
        }
        Token::Identifier if lexer.is_contextual_keyword(b"let") => {
            let mut statement = super::syntax_statement::parse_statement(core, lexer);
            mark_declaration_exported(&mut statement);
            statement
        }
        Token::Identifier if lexer.is_contextual_keyword(b"async") => {
            let mut statement = super::syntax_statement::parse_statement(core, lexer);
            if !matches!(statement.data.as_deref(), Some(StmtData::Function(_))) {
                lexer.unexpected();
            }
            mark_declaration_exported(&mut statement);
            statement
        }
        Token::Default => parse_export_default(core, lexer, loc),
        Token::Asterisk => parse_export_star(core, lexer, loc),
        Token::OpenBrace => {
            let (items, is_single_line, had_type_only_items) = parse_clause(core, lexer, true);
            if lexer.is_contextual_keyword(b"from") {
                lexer.next();
                let (path_range, path) = parse_path(lexer);
                lexer.expect_or_insert_semicolon();
                if had_type_only_items && items.is_empty() {
                    core.has_type_script_export = true;
                    return Stmt::new(
                        loc,
                        StmtData::TypeScript(crate::internal::js_ast::TypeScriptStmt::default()),
                    );
                }
                let namespace_ref = core.new_symbol(
                    SymbolKind::Other,
                    format!("import_{}", generate_non_unique_name_from_path(&path)),
                );
                let import_record_index =
                    add_import_record(core, path_range, path, ImportRecordFlags::default());
                Stmt::new(
                    loc,
                    StmtData::ExportFrom(ExportFromStmt {
                        items,
                        namespace_ref,
                        import_record_index,
                        is_single_line,
                    }),
                )
            } else {
                lexer.expect_or_insert_semicolon();
                if had_type_only_items && items.is_empty() {
                    core.has_type_script_export = true;
                    return Stmt::new(
                        loc,
                        StmtData::TypeScript(crate::internal::js_ast::TypeScriptStmt::default()),
                    );
                }
                Stmt::new(
                    loc,
                    StmtData::ExportClause(ExportClauseStmt {
                        items,
                        is_single_line,
                    }),
                )
            }
        }
        _ => lexer.unexpected(),
    }
}

fn parse_export_default(core: &mut ParserCore, lexer: &mut Lexer, loc: Loc) -> Stmt {
    let default_loc = lexer.loc();
    lexer.expect(Token::Default);
    let value = if lexer.token == Token::Function {
        let expression = parse_function_declaration_prefix(core, lexer)
            .expect("function token was already checked");
        let crate::internal::js_ast::ExprData::Function(function) =
            *expression.data.expect("function expression has data")
        else {
            unreachable!("function prefix returns a function");
        };
        Stmt::new(
            loc,
            StmtData::Function(crate::internal::js_ast::FunctionStmt {
                function: function.function,
                ..crate::internal::js_ast::FunctionStmt::default()
            }),
        )
    } else if lexer.token == Token::Class {
        let expression = parse_class_prefix(core, lexer).expect("class token was already checked");
        let crate::internal::js_ast::ExprData::Class(class) =
            *expression.data.expect("class expression has data")
        else {
            unreachable!("class prefix returns a class");
        };
        Stmt::new(
            loc,
            StmtData::Class(crate::internal::js_ast::ClassStmt {
                class: class.class,
                ..crate::internal::js_ast::ClassStmt::default()
            }),
        )
    } else {
        let value = parse_expression(core, lexer, Precedence::Comma, true);
        lexer.expect_or_insert_semicolon();
        Stmt::new(
            loc,
            StmtData::Expr(ExprStmt {
                value,
                ..ExprStmt::default()
            }),
        )
    };

    let existing_name = match value.data.as_deref() {
        Some(StmtData::Function(function)) => function.function.name,
        Some(StmtData::Class(class)) => class.class.name,
        _ => None,
    };
    let default_name = existing_name.unwrap_or_else(|| LocRef {
        loc: default_loc,
        reference: core.new_symbol(SymbolKind::Other, "default"),
    });
    Stmt::new(
        loc,
        StmtData::ExportDefault(ExportDefaultStmt {
            value,
            default_name,
        }),
    )
}

fn parse_export_star(core: &mut ParserCore, lexer: &mut Lexer, loc: Loc) -> Stmt {
    lexer.expect(Token::Asterisk);
    let alias = if lexer.is_contextual_keyword(b"as") {
        lexer.next();
        let alias_loc = lexer.loc();
        let (name, _) = clause_alias(lexer);
        lexer.next();
        Some(ExportStarAlias {
            original_name: name,
            loc: alias_loc,
        })
    } else {
        None
    };
    lexer.expect_contextual_keyword(b"from");
    let (path_range, path) = parse_path(lexer);
    lexer.expect_or_insert_semicolon();
    let namespace_ref = core.new_symbol(
        SymbolKind::Other,
        format!("{}_star", generate_non_unique_name_from_path(&path)),
    );
    let import_record_index =
        add_import_record(core, path_range, path, ImportRecordFlags::default());
    Stmt::new(
        loc,
        StmtData::ExportStar(ExportStarStmt {
            alias,
            namespace_ref,
            import_record_index,
        }),
    )
}

fn parse_clause(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    is_export: bool,
) -> (Vec<ClauseItem>, bool, bool) {
    lexer.expect(Token::OpenBrace);
    let mut is_single_line = !lexer.has_newline_before;
    let mut items = Vec::new();
    let mut had_type_only_items = false;
    while lexer.token != Token::CloseBrace {
        let mut is_type_only = false;
        let mut prefetched_type = None;
        if core.options.ts.parse && lexer.is_contextual_keyword(b"type") {
            let type_loc = lexer.loc();
            let type_ref = lexer.identifier.clone();
            lexer.next();
            if matches!(lexer.token, Token::Comma | Token::CloseBrace)
                || lexer.is_contextual_keyword(b"as")
            {
                prefetched_type = Some((type_loc, type_ref));
            } else {
                is_type_only = true;
                had_type_only_items = true;
            }
        }
        let name_loc = prefetched_type
            .as_ref()
            .map_or_else(|| lexer.loc(), |item| item.0);
        let source_can_be_local_name =
            prefetched_type.is_some() || lexer.token == Token::Identifier;
        let (mut alias, alias_ref) = if let Some((_, type_ref)) = prefetched_type {
            ("type".into(), type_ref)
        } else {
            let item = clause_alias(lexer);
            lexer.next();
            item
        };
        let original_name = alias.clone();
        let mut alias_loc = name_loc;
        let mut name = LocRef {
            loc: name_loc,
            reference: core.store_name_in_ref(alias_ref),
        };

        if lexer.is_contextual_keyword(b"as") {
            lexer.next();
            let target_loc = lexer.loc();
            if !is_export && lexer.token != Token::Identifier {
                lexer.expected(Token::Identifier);
            }
            let (target, target_ref) = clause_alias(lexer);
            lexer.next();
            if is_export {
                alias = target;
                alias_loc = target_loc;
            } else {
                name = LocRef {
                    loc: target_loc,
                    reference: core.store_name_in_ref(target_ref),
                };
            }
        } else if !is_export && !source_can_be_local_name {
            lexer.expected_string("\"as\"");
        }
        if !is_type_only {
            items.push(ClauseItem {
                alias,
                original_name: if is_export {
                    original_name
                } else {
                    name_for_ref(core, name.reference)
                },
                alias_loc,
                name,
            });
        }

        if lexer.token != Token::Comma {
            break;
        }
        if lexer.has_newline_before {
            is_single_line = false;
        }
        lexer.next();
        if lexer.has_newline_before {
            is_single_line = false;
        }
    }
    if lexer.has_newline_before {
        is_single_line = false;
    }
    lexer.expect(Token::CloseBrace);
    (items, is_single_line, had_type_only_items)
}

fn clause_alias(lexer: &mut Lexer) -> (String, MaybeSubstring) {
    if lexer.token == Token::StringLiteral {
        let text = utf16_to_string(lexer.string_literal());
        let name = String::from_utf8_lossy(&text).into_owned();
        return (name, MaybeSubstring::from_allocated(text));
    }
    if !lexer.is_identifier_or_keyword() {
        lexer.expected(Token::Identifier);
    }
    (
        String::from_utf8_lossy(lexer.raw()).into_owned(),
        lexer.identifier.clone(),
    )
}

fn name_for_ref(core: &ParserCore, reference: crate::internal::ast::Ref) -> String {
    String::from_utf8_lossy(core.load_name_from_ref(reference)).into_owned()
}

fn parse_path(lexer: &mut Lexer) -> (crate::internal::logger::Range, String) {
    if lexer.token != Token::StringLiteral {
        lexer.expected(Token::StringLiteral);
    }
    let range = lexer.range();
    let path = String::from_utf8_lossy(&utf16_to_string(lexer.string_literal())).into_owned();
    lexer.next();
    (range, path)
}

fn add_import_record(
    core: &mut ParserCore,
    range: crate::internal::logger::Range,
    path: String,
    flags: ImportRecordFlags,
) -> u32 {
    let index = u32::try_from(core.import_records.len()).expect("import record count fits in u32");
    core.import_records.push(ImportRecord {
        path: Path {
            text: path,
            ..Path::default()
        },
        range,
        flags,
        kind: ImportKind::Stmt,
        ..ImportRecord::default()
    });
    index
}

fn mark_declaration_exported(statement: &mut Stmt) {
    match statement.data.as_deref_mut() {
        Some(StmtData::Local(local)) => local.is_export = true,
        Some(StmtData::Function(function)) => function.is_export = true,
        Some(StmtData::Class(class)) => class.is_export = true,
        Some(StmtData::Enum(enumeration)) => enumeration.is_export = true,
        _ => {}
    }
}
