#![allow(dead_code)]

use std::collections::HashMap;

use crate::internal::{
    ast::{
        AssertOrWithEntry, AssertOrWithKeyword, ImportAssertOrWith, ImportKind, ImportPhase,
        ImportRecord, ImportRecordFlags, LocRef, SymbolKind,
    },
    helpers::{string_to_utf16, utf16_to_string},
    js_ast::{
        Binding, BindingData, ClauseItem, Decl, ExportClauseStmt, ExportDefaultStmt,
        ExportEqualsStmt, ExportFromStmt, ExportStarAlias, ExportStarStmt, ExprData, ExprStmt,
        IdentifierBinding, ImportStmt, LocalKind, LocalStmt, Precedence, Stmt, StmtData,
        generate_non_unique_name_from_path,
    },
    js_lexer::{Lexer, MaybeSubstring, Token},
    logger::{Loc, Path},
};

use super::{
    parser_core::ParserCore,
    syntax_class::parse_class_prefix,
    syntax_expression::{parse_expression, parse_expression_suffix},
    syntax_function::{parse_async_statement_prefix, parse_function_declaration_prefix},
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
    let preconsumed_default = if core.options.ts.parse && lexer.token == Token::Identifier {
        let name = LocRef {
            loc: lexer.loc(),
            reference: core.store_name_in_ref(lexer.identifier.clone()),
        };
        lexer.next();
        if lexer.token == Token::Equals {
            return parse_type_script_import_equals(core, lexer, loc, name);
        }
        Some(name)
    } else {
        None
    };
    if !core.is_current_scope_module_scope() {
        core.add_error_range(
            crate::internal::logger::Range { loc, len: 6 },
            "An import declaration can only be used at the top level of a module",
        );
    }

    let mut statement = ImportStmt::default();
    let mut phase = ImportPhase::Evaluation;
    let mut clause_consumed = false;
    let mut was_bare = false;
    if let Some(default_name) = preconsumed_default {
        statement.default_name = Some(default_name);
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
    } else {
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
                let (items, is_single_line, _) = parse_clause(core, lexer, false);
                statement.items = Some(items);
                statement.is_single_line = is_single_line;
                lexer.expect_contextual_keyword(b"from");
            }
            Token::Identifier => {
                let first_name = lexer.identifier.clone();
                let first_loc = lexer.loc();
                let is_defer = lexer.raw() == b"defer";
                let is_source = lexer.raw() == b"source";
                lexer.next();
                if is_defer && lexer.token == Token::Asterisk {
                    phase = ImportPhase::Defer;
                    clause_consumed = true;
                    lexer.next();
                    lexer.expect_contextual_keyword(b"as");
                    statement.namespace_ref = core.store_name_in_ref(lexer.identifier.clone());
                    statement.star_name_loc = Some(lexer.loc());
                    lexer.expect(Token::Identifier);
                    lexer.expect_contextual_keyword(b"from");
                } else if is_source && lexer.token == Token::Identifier {
                    phase = ImportPhase::Source;
                    clause_consumed = true;
                    if lexer.raw() == b"from" {
                        let name = LocRef {
                            loc: lexer.loc(),
                            reference: core.store_name_in_ref(lexer.identifier.clone()),
                        };
                        lexer.next();
                        if lexer.is_contextual_keyword(b"from") {
                            statement.default_name = Some(name);
                            lexer.next();
                        } else {
                            phase = ImportPhase::Evaluation;
                            statement.default_name = Some(LocRef {
                                loc: first_loc,
                                reference: core.store_name_in_ref(first_name),
                            });
                        }
                    } else {
                        statement.default_name = Some(LocRef {
                            loc: lexer.loc(),
                            reference: core.store_name_in_ref(lexer.identifier.clone()),
                        });
                        lexer.next();
                        lexer.expect_contextual_keyword(b"from");
                    }
                } else {
                    statement.default_name = Some(LocRef {
                        loc: first_loc,
                        reference: core.store_name_in_ref(first_name),
                    });
                }
                if !clause_consumed && statement.default_name.is_some() {
                    if lexer.token == Token::Comma {
                        lexer.next();
                        match lexer.token {
                            Token::Asterisk => {
                                lexer.next();
                                lexer.expect_contextual_keyword(b"as");
                                statement.namespace_ref =
                                    core.store_name_in_ref(lexer.identifier.clone());
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
            }
            _ => lexer.unexpected(),
        }
    }

    let (path_range, path, assert_or_with, path_flags) = parse_path(core, lexer);
    lexer.expect_or_insert_semicolon();
    if core.options.ts.parse
        && core.options.ts.config.unused_import_flags()
            == crate::internal::config::TsUnusedImportFlags::KEEP_VALUES
        && statement.items.as_ref().is_some_and(Vec::is_empty)
    {
        if statement.default_name.is_none() {
            core.has_type_script_export = true;
            return Stmt::new(
                loc,
                StmtData::TypeScript(crate::internal::js_ast::TypeScriptStmt::default()),
            );
        }
        statement.items = None;
    }
    if let Some(star_name_loc) = statement.star_name_loc {
        let name =
            String::from_utf8_lossy(core.load_name_from_ref(statement.namespace_ref)).into_owned();
        statement.namespace_ref = core.declare_symbol(SymbolKind::Import, star_name_loc, &name);
    } else {
        statement.namespace_ref = core.new_symbol(
            SymbolKind::Other,
            format!("import_{}", generate_non_unique_name_from_path(&path)),
        );
        core.current_scope
            .as_ref()
            .expect("imports require a current scope")
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generated
            .push(statement.namespace_ref);
    }

    if let Some(default_name) = &mut statement.default_name {
        let name =
            String::from_utf8_lossy(core.load_name_from_ref(default_name.reference)).into_owned();
        default_name.reference = core.declare_symbol(SymbolKind::Import, default_name.loc, &name);
        core.is_import_item.insert(default_name.reference);
    }
    if let Some(items) = &mut statement.items {
        for item in items {
            let name =
                String::from_utf8_lossy(core.load_name_from_ref(item.name.reference)).into_owned();
            item.name.reference = core.declare_symbol(SymbolKind::Import, item.name.loc, &name);
            core.is_import_item.insert(item.name.reference);
        }
    }
    let mut flags = if was_bare {
        ImportRecordFlags::WAS_ORIGINALLY_BARE_IMPORT
    } else {
        ImportRecordFlags::default()
    };
    flags |= path_flags;
    statement.import_record_index =
        add_import_record(core, path_range, path, assert_or_with, flags);
    core.import_records[statement.import_record_index as usize].phase = phase;
    Stmt::new(loc, StmtData::Import(statement))
}

fn parse_type_script_import_equals(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    loc: Loc,
    name: LocRef,
) -> Stmt {
    lexer.expect(Token::Equals);
    let value = parse_expression(core, lexer, Precedence::Lowest, true);
    lexer.expect_or_insert_semicolon();
    Stmt::new(
        loc,
        StmtData::Local(LocalStmt {
            declarations: vec![Decl {
                binding: Binding {
                    loc: name.loc,
                    data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                        reference: name.reference,
                    }))),
                },
                value_or_nil: value,
            }],
            kind: LocalKind::Const,
            was_ts_import_equals: true,
            ..LocalStmt::default()
        }),
    )
}

#[allow(clippy::too_many_lines)]
pub(crate) fn parse_export_statement(core: &mut ParserCore, lexer: &mut Lexer) -> Stmt {
    let loc = lexer.loc();
    let previous_export_keyword = core.esm_export_keyword;
    if core.is_current_scope_module_scope() && core.esm_export_keyword.len == 0 {
        core.esm_export_keyword = crate::internal::logger::Range { loc, len: 6 };
    }
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
    if core.options.ts.parse && lexer.token == Token::Equals {
        core.esm_export_keyword = previous_export_keyword;
        lexer.next();
        let value = parse_expression(core, lexer, Precedence::Lowest, true);
        lexer.expect_or_insert_semicolon();
        return Stmt::new(loc, StmtData::ExportEquals(ExportEqualsStmt { value }));
    }
    match lexer.token {
        Token::Var
        | Token::Const
        | Token::Function
        | Token::Class
        | Token::Enum
        | Token::Import
        | Token::At => {
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
                let (path_range, path, assert_or_with, flags) = parse_path(core, lexer);
                lexer.expect_or_insert_semicolon();
                if had_type_only_items
                    && items.is_empty()
                    && !core
                        .options
                        .ts
                        .config
                        .unused_import_flags()
                        .contains(crate::internal::config::TsUnusedImportFlags::KEEP_STMT)
                {
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
                    add_import_record(core, path_range, path, assert_or_with, flags);
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
                if had_type_only_items
                    && items.is_empty()
                    && !core
                        .options
                        .ts
                        .config
                        .unused_import_flags()
                        .contains(crate::internal::config::TsUnusedImportFlags::KEEP_STMT)
                {
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
    let mut has_no_side_effects_comment = lexer
        .has_comment_before
        .contains(crate::internal::js_lexer::CommentBefore::NO_SIDE_EFFECTS);
    lexer.expect(Token::Default);
    has_no_side_effects_comment |= lexer
        .has_comment_before
        .contains(crate::internal::js_lexer::CommentBefore::NO_SIDE_EFFECTS);
    if let Some(statement) =
        super::syntax_typescript::parse_type_script_statement(core, lexer, true)
    {
        core.has_type_script_export = true;
        return statement;
    }
    let is_abstract_class = core.options.ts.parse && lexer.is_contextual_keyword(b"abstract");
    if is_abstract_class {
        lexer.next();
    }
    let mut value = if lexer.is_contextual_keyword(b"async") {
        let expression =
            parse_async_statement_prefix(core, lexer).expect("async token was already checked");
        if matches!(expression.data.as_deref(), Some(ExprData::Function(_))) {
            let ExprData::Function(function) =
                *expression.data.expect("async function expression has data")
            else {
                unreachable!("async function prefix returns a function");
            };
            Stmt::new(
                loc,
                StmtData::Function(crate::internal::js_ast::FunctionStmt {
                    function: function.function,
                    ..crate::internal::js_ast::FunctionStmt::default()
                }),
            )
        } else {
            let value = parse_expression_suffix(core, lexer, expression, Precedence::Comma, true);
            lexer.expect_or_insert_semicolon();
            Stmt::new(
                loc,
                StmtData::Expr(ExprStmt {
                    value,
                    ..ExprStmt::default()
                }),
            )
        }
    } else if lexer.token == Token::Function {
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

    super::syntax_statement::apply_no_side_effects_comment(
        core,
        &mut value,
        has_no_side_effects_comment,
    );

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
    let (alias, alias_namespace_ref) = if lexer.is_contextual_keyword(b"as") {
        lexer.next();
        let alias_loc = lexer.loc();
        let (name, _) = clause_alias(lexer);
        let namespace_ref = core.new_symbol(SymbolKind::Other, name.clone());
        lexer.next();
        (
            Some(ExportStarAlias {
                original_name: name,
                loc: alias_loc,
            }),
            Some(namespace_ref),
        )
    } else {
        (None, None)
    };
    lexer.expect_contextual_keyword(b"from");
    let (path_range, path, assert_or_with, flags) = parse_path(core, lexer);
    lexer.expect_or_insert_semicolon();
    let namespace_ref = alias_namespace_ref.unwrap_or_else(|| {
        core.new_symbol(
            SymbolKind::Other,
            format!("{}_star", generate_non_unique_name_from_path(&path)),
        )
    });
    let import_record_index = add_import_record(core, path_range, path, assert_or_with, flags);
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

fn parse_path(
    core: &mut ParserCore,
    lexer: &mut Lexer,
) -> (
    crate::internal::logger::Range,
    String,
    Option<ImportAssertOrWith>,
    ImportRecordFlags,
) {
    if lexer.token != Token::StringLiteral {
        lexer.expected(Token::StringLiteral);
    }
    let range = lexer.range();
    let path = String::from_utf8_lossy(&utf16_to_string(lexer.string_literal())).into_owned();
    lexer.next();
    let mut flags = ImportRecordFlags::default();
    let mut assert_or_with = None;
    if lexer.token == Token::With
        || (!lexer.has_newline_before && lexer.is_contextual_keyword(b"assert"))
    {
        let keyword = if lexer.token == Token::With {
            AssertOrWithKeyword::With
        } else {
            AssertOrWithKeyword::Assert
        };
        let keyword_loc = lexer.loc();
        lexer.next();
        let inner_open_brace_loc = lexer.loc();
        lexer.expect(Token::OpenBrace);
        let mut entries = Vec::new();
        let mut duplicates = HashMap::new();
        while lexer.token != Token::CloseBrace {
            let key_loc = lexer.loc();
            let key_range = lexer.range();
            let (key, key_text, prefer_quoted_key) = if lexer.is_identifier_or_keyword() {
                let key_text = String::from_utf8_lossy(lexer.raw()).into_owned();
                (string_to_utf16(key_text.as_bytes()), key_text, false)
            } else if lexer.token == Token::StringLiteral {
                let key = lexer.string_literal().to_vec();
                let key_text = String::from_utf8_lossy(&utf16_to_string(&key)).into_owned();
                (key, key_text, !core.options.minify_syntax)
            } else {
                lexer.expected(Token::Identifier);
            };
            if duplicates.insert(key_text.clone(), key_range).is_some() {
                core.add_error_range(
                    key_range,
                    format!(
                        "Duplicate import {} {key_text:?}",
                        if keyword == AssertOrWithKeyword::Assert {
                            "assertion"
                        } else {
                            "attribute"
                        }
                    ),
                );
            }
            lexer.next();
            lexer.expect(Token::Colon);
            let value_loc = lexer.loc();
            let value = lexer.string_literal().to_vec();
            lexer.expect(Token::StringLiteral);
            if keyword == AssertOrWithKeyword::Assert
                && key_text == "type"
                && utf16_to_string(&value) == b"json"
            {
                flags |= ImportRecordFlags::ASSERT_TYPE_JSON;
            }
            entries.push(AssertOrWithEntry {
                key,
                value,
                key_loc,
                value_loc,
                prefer_quoted_key,
            });
            if lexer.token != Token::Comma {
                break;
            }
            lexer.next();
        }
        let inner_close_brace_loc = lexer.loc();
        lexer.expect(Token::CloseBrace);
        assert_or_with = Some(ImportAssertOrWith {
            entries,
            keyword_loc,
            inner_open_brace_loc,
            inner_close_brace_loc,
            keyword,
            ..ImportAssertOrWith::default()
        });
    }
    (range, path, assert_or_with, flags)
}

fn add_import_record(
    core: &mut ParserCore,
    range: crate::internal::logger::Range,
    path: String,
    assert_or_with: Option<ImportAssertOrWith>,
    flags: ImportRecordFlags,
) -> u32 {
    let index = u32::try_from(core.import_records.len()).expect("import record count fits in u32");
    core.import_records.push(ImportRecord {
        assert_or_with,
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
